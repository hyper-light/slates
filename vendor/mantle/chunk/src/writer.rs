//! The group-commit loop: one per volume (docs/design/chunk-store.md §4).
//!
//! It takes every request that arrived while the previous batch was being made durable, the
//! zero-timer group commit of Helland et al. (Tandem TR 88.1, §2): one log write and flush
//! commits the group (DeWitt et al., SIGMOD 1984, §5.2). It validates them against the
//! index, lays their records into open segments, writes the records and one index frame, and
//! flushes the device once.
//! Only then does it publish the batch to readers and answer the requests. A failed write or
//! flush fences the volume; the flush is never retried (Rebello et al., ATC 2020).
//!
//! A batch that has just been answered frees its submitters to send their next requests.
//! Formed at once from whatever is queued, the next batch would miss them, and a few
//! closed-loop submitters would alternate between batches with every request waiting for two
//! flushes (docs/measurements/2026-09-28-chunk-store-benchmark.md, finding 5). The writer
//! waits for them as long as waiting is expected to lower total latency, a wait derived from
//! the measured service time and the learned share of submitters that return
//! (`hyper_block::commit`).
//!
//! Records go to one of three streams, each with its own open segment: new writes, new writes
//! too large to share a segment with another of their size, and the cleaner's relocations.
//! Keeping relocated data apart groups data by age, which is what lets later cleaning find
//! segments that are mostly dead (Rosenblum and Ousterhout, TOCS 1992, §3.6). A record longer
//! than half a segment shares one with no record of its own size, so among small ones it only
//! sealed a segment they had begun nearly empty when it did not fit after them: a 64 KiB record
//! ahead of an 8 MiB one sealed a segment of 8 MiB with 64 KiB in it, and near full the cleaner
//! won such segments back a pass at a time, holding new writes for each pass, so a full volume
//! answered them `Busy` rather than `Full` (docs/measurements/2026-10-28-transfer-scale.md).

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, RwLock};

use hyper_block::block::BlockFile;
use hyper_block::buf::{AlignedBuf, Alignment, Pool};
use hyper_block::commit::Anticipation;
use hyper_block::issuer::Attached;

use crate::error::ChunkError;
use crate::frame::{
    self, DeleteRecord, KIND_BATCH, KIND_CHECKPOINT_BEGIN, KIND_CHECKPOINT_CHUNK,
    KIND_CHECKPOINT_END, KIND_WRAP, LogRecord, PutRecord, SegmentRecord, SegmentState,
};
use crate::index::{Fragment, Index, Inserted, SegmentInfo, Segments};
use crate::key::ChunkKey;
use crate::layout::{
    Config, FRAME_PAYLOAD, Geometry, MAX_FRAME_BYTES, batch_frame_bytes, checkpoint_bytes,
    largest_frame,
};
use crate::log::Cursor;
use crate::record::{self, FLAG_FINAL, Payload, RecordHeader, SegmentHeader, Sink};
use crate::recover::{Identity, ReadPool, verify_at};
use crate::superblock::Superblock;

/// Free segments kept back from new writes so the cleaner can always relocate into one
/// (Rosenblum and Ousterhout, TOCS 1992, §3.6: the cleaner needs clean segments to work).
pub(crate) const CLEANER_RESERVE: usize = 1;

/// Record sequences and segment incarnations reserved at a time. The superblock records how
/// far they are reserved before any record carries a number beyond it, and recovery resumes
/// above the reservation. A freed segment keeps its records until they are overwritten, and
/// a batch whose flush failed may have left records on the device that the log never names;
/// numbers taken from what the log remembers could repeat theirs, and a reused incarnation
/// would let roll-forward take a stale record for a new one. Recovery skips what was
/// reserved but not used, the way Percolator's timestamp oracle does (docs/research/11 §11).
///
/// A reservation costs one superblock write and flush, `c`, so reserving `R` at a time for
/// numbers issued at rate `r` spends a fraction `c·r/R` of the writer's time on it. With the
/// measured durable 4 KiB write, `c` = 4.7 ms: 2^24 sequences keep that fraction under 10^-3
/// up to 3.6M puts a second, 1,000 times this machine's measured 3.7K, and 2^16 incarnations
/// do so to 14K new segments a second, where 3 GB/s into 256 MiB segments opens 11. Each
/// crash skips at most one reservation of 2^64, so neither space can run out.
pub(crate) const SEQUENCE_RESERVE: u64 = 1 << 24;
pub(crate) const INCARNATION_RESERVE: u64 = 1 << 16;

pub(crate) enum Op {
    Write {
        key: ChunkKey,
        offset: u64,
        payload: Payload,
        seal: bool,
    },
    Delete {
        key: ChunkKey,
    },
    /// Moves fragments' verified bytes to the cleaner's stream, together in one batch so they
    /// pack densely. Each move is applied only if its chunk still holds exactly that fragment,
    /// so a concurrent delete or rewrite wins.
    Relocate {
        moves: Vec<Move>,
    },
    /// Writes the whole index into the log after the batch it arrives in.
    Checkpoint,
    /// Runs a batch that writes nothing of its own, so sealed segments left with nothing
    /// live are freed in its frame; answered once that frame is durable. The cleaner sends
    /// it after each victim.
    Free,
}

/// One fragment the cleaner moves.
pub(crate) struct Move {
    pub key: ChunkKey,
    pub from: Fragment,
    pub payload: Payload,
    pub flags: u8,
    pub time_ns: u64,
}

pub(crate) struct Request {
    pub op: Op,
    pub reply: Reply,
    /// Payload bytes the request holds of the client queue's room (`Queue`); `None` for the
    /// cleaner's requests, which it sends one at a time outside that room.
    pub queued: Option<u64>,
}

/// The room client requests hold in the writer's queue (docs/design/chunk-store.md §4).
#[derive(Debug, Default)]
pub(crate) struct Queue {
    requests: usize,
    bytes: u64,
}

impl Queue {
    /// Takes room for a request of `bytes` payload bytes, or refuses with `Busy`. A payload
    /// larger than the whole byte bound is admitted into a queue holding no other payload.
    pub fn admit(&mut self, bytes: u64, limits: &crate::layout::Limits) -> Result<(), ChunkError> {
        let requests = self.requests.checked_add(1).ok_or(ChunkError::Busy)?;
        let total = self.bytes.checked_add(bytes).ok_or(ChunkError::Busy)?;
        if requests > limits.queue_requests() || (self.bytes > 0 && total > limits.queue_bytes()) {
            return Err(ChunkError::Busy);
        }
        self.requests = requests;
        self.bytes = total;
        Ok(())
    }

    /// Takes back the room of a request the writer holds past its batch (`Writer::waiting`): it
    /// is the queue's until it is answered, so the queue's bound counts it.
    pub fn hold(&mut self, bytes: u64) {
        self.requests = self.requests.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
    }

    /// Gives back the room of a request that left the queue or was never sent.
    pub fn release(&mut self, bytes: u64) {
        self.requests = self.requests.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(bytes);
    }

    /// Requests queued and not yet taken by the writer.
    #[cfg(test)]
    pub fn requests(&self) -> usize {
        self.requests
    }
}

/// What readers, the writer and the cleaner share.
pub(crate) struct Shared<F> {
    pub file: F,
    pub geometry: Geometry,
    pub volume: u128,
    pub checksum_shift: u8,
    pub index: RwLock<Index>,
    pub fenced: std::sync::atomic::AtomicBool,
    /// Set when the volume closes: background threads stop at their next wake.
    pub stopping: std::sync::atomic::AtomicBool,
    /// Bytes of records deleted since the volume opened: the cleaner waits for this to grow
    /// after a pass that could not gain space. Relocations do not count; they free nothing.
    pub dead_bytes: std::sync::atomic::AtomicU64,
    /// `dead_bytes` when the cleaner's last pass gained no segment, `u64::MAX` if it gained or
    /// none has run: while the two are equal, cleaning cannot free space and a client write
    /// with no segment to go to is `Full` rather than `Busy`.
    pub futile_at: std::sync::atomic::AtomicU64,
    /// Requests submitted to the writer so far, counted just before each is sent.
    pub submitted: std::sync::atomic::AtomicU64,
    /// Segments the cleaner's stream has opened since the volume opened: the free space
    /// cleaning spends, which the cleaner weighs against the victims it frees. Client writes
    /// spend free space during a pass too, so the free count alone does not say what the
    /// pass gained.
    pub clean_opened: std::sync::atomic::AtomicU64,
    /// The segment the cleaner's stream has open as of the last durable batch, `u64::MAX` when
    /// none: where the cleaner's next relocation goes, which its plan of a pass starts from.
    pub clean_open: std::sync::atomic::AtomicU64,
    /// Checkpoints written since the volume opened.
    pub checkpoints: std::sync::atomic::AtomicU64,
    /// The room client requests hold in the writer's queue.
    pub queue: std::sync::Mutex<Queue>,
    /// The fastest the writer has laid down payload, in bytes a second: a batch's payload
    /// over its service time, the largest since the volume opened.
    pub peak_rate: std::sync::atomic::AtomicU64,
    /// The writer's moving average of a batch's service time, in nanoseconds.
    pub service_ns: std::sync::atomic::AtomicU64,
    /// The longest the cleaner has taken to clean one victim, in nanoseconds.
    pub clean_ns: std::sync::atomic::AtomicU64,
    /// Clean below this many free segments (`runway`).
    pub low_water: std::sync::atomic::AtomicUsize,
    /// Held through a cleaning pass: the background cleaner's or one asked for.
    pub cleaning: std::sync::Mutex<()>,
    /// The segment table as of the last durable batch.
    pub usage: RwLock<Segments>,
    /// Holds client reads at the device's measured depth.
    pub reads: crate::read::Gate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Client = 0,
    Clean = 1,
    /// New writes whose record is longer than half a segment: one to a segment.
    Whole = 2,
}

impl Stream {
    /// The stream a new write's record of `len` bytes goes to in segments of `segment` bytes.
    fn of_write(len: u64, segment: u64) -> Self {
        if len > segment / 2 {
            Self::Whole
        } else {
            Self::Client
        }
    }
}

/// The writer's state, owned by its thread.
pub(crate) struct Writer<F: BlockFile> {
    pub shared: Arc<Shared<F>>,
    pub config: Config,
    pub rx: Receiver<Request>,
    pub segments: Segments,
    /// The open segment of each stream.
    pub opens: [Option<u32>; 3],
    pub incarnation: u64,
    pub sequence: u64,
    pub cursor: Cursor,
    pub superblock: Superblock,
    pub fragments: u64,
    /// Wakes the cleaner when free segments run low.
    pub poke: Option<SyncSender<()>>,
    /// Buffers for the batches the writer lays out, reused from batch to batch.
    pub pool: Pool,
    /// The writer thread's own read buffers, for the stored copy a retry is compared with:
    /// recovery's, handed on when the volume starts.
    pub reads: ReadPool,
    /// The device's issuer, which every write and flush of the volume goes through
    /// (docs/design/chunk-store.md §4).
    pub io: Attached,
    /// What the writer has learned of its batches' service time and of the submitters that
    /// return while it waits.
    pub anticipation: Anticipation,
    /// Requests received from the queue so far.
    pub received: u64,
    /// Answers held until a later frame confirms the last batch's: its deletes and the puts
    /// in segments it opened, and every answer that rests on them (`Writer::settle`). At most
    /// one batch's requests and the next one's.
    pub unconfirmed: Vec<Held>,
    /// Client writes that found no free segment while cleaning may still free one, held for the
    /// cleaner's verdict, each holding its room of the queue (`Queue::hold`), so the queue's
    /// bound is theirs too.
    pub waiting: Waiting,
}

/// The client writes a writer holds for the cleaner's verdict, and what the first of them was
/// held against: the verdict and the free segments its hold read. They run again, all of them,
/// once either differs from that (`Writer::retry_waiting`), so a verdict given at any time after
/// one of them was found blocked runs it again. Recorded as the latest hold read it, a verdict
/// given between a write's check and its hold, or before a later write was held, was taken for
/// one already seen: the write waited for another, the cleaner idle with its verdict given, and
/// the volume's test of the planned pass hung on CI's one-CPU runner for five hours.
#[derive(Default)]
pub(crate) struct Waiting {
    requests: Vec<Request>,
    since: (u64, usize),
}

impl Waiting {
    /// Holds `request`, found blocked against `seen`, the verdict and the free segments its
    /// check read.
    fn hold(&mut self, request: Request, seen: (u64, usize)) {
        if self.requests.is_empty() {
            self.since = seen;
        }
        self.requests.push(request);
    }

    /// Every held write, to run again, once `now` differs from what the first was held against.
    fn due(&mut self, now: (u64, usize)) -> Option<Vec<Request>> {
        if self.requests.is_empty() || now == self.since {
            return None;
        }
        Some(std::mem::take(&mut self.requests))
    }

    /// Every held write, the writer ending.
    fn take(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }
}

/// A chunk as the batch being validated sees it: the index plus earlier requests in the batch.
#[derive(Clone)]
struct View {
    len: u64,
    sealed: bool,
    fragments: usize,
    /// Fragments added earlier in this batch: (chunk offset, length, CRC-32C).
    pending: Vec<(u64, u64, u32)>,
    /// A request earlier in this batch changed the chunk.
    touched: bool,
}

/// How a durable fragment compares with a retry's bytes.
enum Stored {
    Same,
    Differs,
    /// It does not verify, or cannot be read.
    Damaged,
}

enum Decision {
    /// Write a fragment whose payload has this CRC-32C.
    Write(u32),
    /// Relocate the moves at these positions; the others' chunks changed.
    Relocate(Vec<usize>),
    Delete,
    /// Free what the batch can (`Op::Free`).
    Free,
    /// Nothing to write: the request is already satisfied (a retry, a relocation of a chunk
    /// that changed, or deleting nothing).
    Done,
}

struct Accepted {
    request: Request,
    decision: Decision,
}

/// An answer held until what it rests on is durable and confirmed.
pub(crate) type Held = (Reply, Result<(), ChunkError>);

/// A request validation answered without writing, and what its answer rests on.
struct Answered {
    reply: Reply,
    result: Result<(), ChunkError>,
    /// The accepted request of this batch that last changed the chunk, when the answer was
    /// decided on that change: the answer stands only if it is written.
    leader: Option<usize>,
}

/// One contiguous run of one segment written by this batch: where it starts and ends, and
/// what goes in it, in order. It is encoded only once the whole batch is placed, straight
/// into one aligned buffer of exactly its size.
struct Region {
    segment: u32,
    /// Where the region starts in the segment, block-aligned.
    start: u64,
    /// Where its last record ends.
    end: u64,
    parts: Vec<Part>,
}

enum Part {
    /// A newly opened segment's first block.
    Segment(SegmentHeader),
    /// A data record; `payload` indexes the batch's payloads.
    Record {
        header: RecordHeader,
        payload: usize,
    },
}

/// One batch's placement of records into segments.
struct Layout {
    geometry: Geometry,
    volume: u128,
    now: u64,
    records: Vec<LogRecord>,
    regions: Vec<Region>,
    /// Where each touched segment's next record goes.
    positions: HashMap<u32, u64>,
    /// Segments this batch opened, with their incarnations.
    opened: Vec<(u32, u64)>,
    /// How many of them the cleaner's stream opened.
    clean_opened: u64,
    sealed: Vec<(u32, u32)>,
    freed: Vec<u32>,
    opens: [Option<u32>; 3],
    incarnation: u64,
    /// The free segments this batch may open, lowest last.
    free: Vec<u32>,
    /// Free segments left, counting those `free` does not list.
    free_left: usize,
    /// Incarnation and write position before this batch of each segment it may seal or
    /// free.
    before: HashMap<u32, (u64, u64)>,
}

impl Layout {
    fn incarnation_of(&self, segment: u32) -> u64 {
        self.opened
            .iter()
            .rev()
            .find(|(s, _)| *s == segment)
            .map(|(_, i)| *i)
            .or_else(|| self.before.get(&segment).map(|(i, _)| *i))
            .unwrap_or(0)
    }

    /// Finds room for `len` bytes in `stream`, sealing its segment and opening another when
    /// it is full. New writes leave `CLEANER_RESERVE` free segments to the cleaner.
    fn place(&mut self, stream: Stream, len: u64) -> Option<(u32, u64)> {
        let block = self.geometry.block;
        let slot = stream as usize;
        if let Some(s) = self.opens.get(slot).copied().flatten() {
            let written = self.before.get(&s).map_or(block, |(_, w)| (*w).max(block));
            let at = *self.positions.entry(s).or_insert(written);
            if at.saturating_add(len) <= self.geometry.segment_size {
                return Some((s, at));
            }
            let pos = at.checked_next_multiple_of(block).unwrap_or(at);
            let incarnation = self.incarnation_of(s);
            self.records.push(LogRecord::Segment(SegmentRecord {
                segment: s,
                incarnation,
                state: SegmentState::Sealed,
                write_pos: u32::try_from(pos).unwrap_or(u32::MAX),
            }));
            self.sealed
                .push((s, u32::try_from(pos).unwrap_or(u32::MAX)));
            if let Some(o) = self.opens.get_mut(slot) {
                *o = None;
            }
        }
        let reserve = if stream == Stream::Clean {
            0
        } else {
            CLEANER_RESERVE
        };
        if self.free_left <= reserve {
            return None;
        }
        let next = self.free.pop()?;
        self.free_left = self.free_left.saturating_sub(1);
        self.incarnation = self.incarnation.saturating_add(1);
        self.records.push(LogRecord::Segment(SegmentRecord {
            segment: next,
            incarnation: self.incarnation,
            state: SegmentState::Open,
            write_pos: u32::try_from(block).unwrap_or(u32::MAX),
        }));
        let header = SegmentHeader {
            volume: self.volume,
            segment: next,
            incarnation: self.incarnation,
            time_ns: self.now,
        };
        self.regions.push(Region {
            segment: next,
            start: 0,
            end: block,
            parts: vec![Part::Segment(header)],
        });
        self.positions.insert(next, block);
        self.opened.push((next, self.incarnation));
        if stream == Stream::Clean {
            self.clean_opened = self.clean_opened.saturating_add(1);
        }
        if let Some(o) = self.opens.get_mut(slot) {
            *o = Some(next);
        }
        Some((next, block))
    }

    /// Stages a record of `len` bytes at `at` in `segment` and advances the segment's
    /// position.
    fn stage(&mut self, segment: u32, at: u64, len: u64, header: RecordHeader, payload: usize) {
        let region = match self.regions.iter_mut().rev().find(|r| r.segment == segment) {
            Some(region) => region,
            None => {
                self.regions.push(Region {
                    segment,
                    start: at,
                    end: at,
                    parts: Vec::new(),
                });
                match self.regions.last_mut() {
                    Some(region) => region,
                    None => return,
                }
            }
        };
        region.parts.push(Part::Record { header, payload });
        region.end = at.saturating_add(len);
        self.positions.insert(segment, region.end);
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Where a request's answer goes, and the waker told once it is there, so that a caller with
/// many requests out learns of each answer as it comes (docs/design/node.md §1.3).
pub(crate) struct Reply {
    tx: SyncSender<Result<(), ChunkError>>,
    waker: Option<std::task::Waker>,
}

impl Reply {
    pub fn new(tx: SyncSender<Result<(), ChunkError>>, waker: Option<std::task::Waker>) -> Self {
        Self { tx, waker }
    }
}

impl Drop for Reply {
    /// Wakes the submitter once: after its answer is sent, or when the request is dropped
    /// unanswered, which its receiver then reads as closed. The waker is the caller's code, run
    /// on the writer's thread: one that panics is contained here, its payload dropped behind the
    /// same boundary, so the writer goes on answering every other request (CLAUDE.md §1). The
    /// caller's answer is already sent or its request dropped, which its receiver reads.
    fn drop(&mut self) {
        if let Some(waker) = self.waker.take()
            && let Err(payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waker.wake()))
            && let Err(again) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
        {
            // A payload whose own drop panicked left another, the caller's too: it is let go
            // without running its drop, one allocation for a request whose waker does both.
            let _unrun = std::mem::ManuallyDrop::new(again);
        }
    }
}

fn answer(tx: Reply, result: Result<(), ChunkError>) {
    // The submitter may have given up waiting; its answer has nowhere to go.
    let _ = tx.tx.send(result);
}

fn reply(request: Request, result: Result<(), ChunkError>) {
    answer(request.reply, result);
}

impl<F: BlockFile> Writer<F> {
    pub fn run(mut self) {
        // Requests the previous batch answered, and requests already submitted when it did.
        let mut answered = 0u64;
        let mut backlog = 0u64;
        loop {
            let first = match self.rx.try_recv() {
                Ok(first) => first,
                // No batch follows at once to confirm the last one's deletes: a frame of its
                // own does, before the writer waits.
                Err(TryRecvError::Empty) if !self.unconfirmed.is_empty() => {
                    self.confirm();
                    continue;
                }
                Err(TryRecvError::Empty) => match self.rx.recv() {
                    Ok(first) => first,
                    Err(_) => {
                        self.end_waiting(|| ChunkError::Closed);
                        return;
                    }
                },
                Err(TryRecvError::Disconnected) => {
                    self.confirm();
                    self.end_waiting(|| ChunkError::Closed);
                    return;
                }
            };
            self.dequeued(&first);
            let mut bytes = request_bytes(&first);
            let mut batch = vec![first];
            while self.room(&batch, bytes) {
                match self.rx.try_recv() {
                    Ok(next) => {
                        self.dequeued(&next);
                        bytes = bytes.saturating_add(request_bytes(&next));
                        batch.push(next);
                    }
                    Err(_) => break,
                }
            }
            self.gather(&mut batch, &mut bytes, answered, backlog);
            answered = u64::try_from(batch.len()).unwrap_or(u64::MAX);
            if self.shared.fenced.load(Ordering::Acquire) {
                for request in batch {
                    reply(request, Err(ChunkError::Fenced));
                }
                self.end_waiting(|| ChunkError::Fenced);
            } else {
                let lsn = self.cursor.lsn;
                self.process(batch);
                // A batch that wrote no frame confirmed nothing: its requests may have been
                // retries or deletes of nothing, and while such requests keep coming the
                // queue is never empty. The held answers get their frame now, so none waits
                // longer than one batch (audit S15).
                if self.cursor.lsn == lsn {
                    self.confirm();
                }
                self.poke_cleaner();
                self.retry_waiting();
            }
            if self.shared.fenced.load(Ordering::Acquire) {
                for (tx, _) in std::mem::take(&mut self.unconfirmed) {
                    answer(tx, Err(ChunkError::Fenced));
                }
            }
            // Requests submitted before these answers went out are queued ahead of any the
            // answered submitters send next.
            backlog = self
                .shared
                .submitted
                .load(Ordering::Acquire)
                .saturating_sub(self.received);
        }
    }

    /// Holds a client write past its batch until the cleaner's verdict (`waiting`), found
    /// blocked against `verdict`, the one its check read.
    fn wait(&mut self, request: Request, verdict: u64) {
        if let Some(bytes) = request.queued
            && let Ok(mut queue) = self.shared.queue.lock()
        {
            queue.hold(bytes);
        }
        let free = self.segments.free_count();
        self.waiting.hold(request, (verdict, free));
    }

    /// Runs the held writes again once the cleaner gave a verdict or a segment was freed since
    /// the first was held: each written, refused `Full` for good, or held again.
    fn retry_waiting(&mut self) {
        let now = (
            self.shared.futile_at.load(Ordering::Relaxed),
            self.segments.free_count(),
        );
        let Some(batch) = self.waiting.due(now) else {
            return;
        };
        for request in &batch {
            self.release(request);
        }
        self.process(batch);
    }

    /// Answers every held write with the error `error` makes: the volume fenced, or its writer
    /// stopping.
    fn end_waiting(&mut self, error: impl Fn() -> ChunkError) {
        for request in self.waiting.take() {
            self.release(&request);
            reply(request, Err(error()));
        }
    }

    /// Gives back the room a held request took of the queue.
    fn release(&self, request: &Request) {
        if let Some(bytes) = request.queued
            && let Ok(mut queue) = self.shared.queue.lock()
        {
            queue.release(bytes);
        }
    }

    /// Counts a request out of the queue and gives back the room it held there.
    fn dequeued(&mut self, request: &Request) {
        self.received = self.received.saturating_add(1);
        if let Some(bytes) = request.queued
            && let Ok(mut queue) = self.shared.queue.lock()
        {
            queue.release(bytes);
        }
    }

    fn room(&self, batch: &[Request], bytes: usize) -> bool {
        batch.len() < self.config.limits.batch_requests && bytes < self.config.limits.batch_bytes
    }

    /// Waits for the submitters the last batch answered while waiting is expected to lower
    /// total latency (module docs), and learns how many of them return.
    fn gather(&mut self, batch: &mut Vec<Request>, bytes: &mut usize, answered: u64, backlog: u64) {
        if answered == 0 {
            return;
        }
        // The first `backlog` requests received were sent before the answers.
        let returned = |batch: &Vec<Request>| {
            u64::try_from(batch.len())
                .unwrap_or(u64::MAX)
                .saturating_sub(backlog)
        };
        while returned(batch) < answered && self.room(batch, *bytes) {
            let Some(step) = self.anticipation.wait(batch.len()) else {
                break;
            };
            match self.rx.recv_timeout(step) {
                Ok(next) => {
                    self.dequeued(&next);
                    *bytes = bytes.saturating_add(request_bytes(&next));
                    batch.push(next);
                }
                Err(_) => break,
            }
        }
        self.anticipation.learn(answered, returned(batch));
    }

    fn fence(&self) {
        self.shared.fenced.store(true, Ordering::Release);
    }

    /// Confirms the last batch's frame with a later one, flushed, and answers its deletes: a
    /// checkpoint when one is due, which the log's reserve holds room for, and otherwise an
    /// empty frame, which the reserve for a batch's frame holds room for. A delete lives only
    /// in its frame, where a put's record describes itself in its segment, so an answered
    /// delete must be in a frame recovery never takes for a torn tail: one a later frame
    /// follows (docs/design/chunk-store.md §6; audit S15).
    fn confirm(&mut self) {
        if self.unconfirmed.is_empty() {
            return;
        }
        let result = if self.shared.fenced.load(Ordering::Acquire) {
            Err(ChunkError::Fenced)
        } else if self.checkpoint_due(0) {
            self.checkpoint()
        } else {
            let group = self.cursor.lsn;
            self.append_frame(KIND_BATCH, &[], group, true).map(|_| ())
        };
        let result = match result {
            Ok(()) => Ok(()),
            Err(e) => {
                self.fence();
                Err(e)
            }
        };
        let mut first = result.err();
        for (tx, held) in std::mem::take(&mut self.unconfirmed) {
            match first.take() {
                Some(e) => answer(tx, Err(e)),
                None if self.shared.fenced.load(Ordering::Acquire) => {
                    answer(tx, Err(ChunkError::Fenced));
                }
                None => answer(tx, held),
            }
        }
    }

    /// The last batch's frame has a later one, durable: the answers held for it go out.
    fn confirmed(&mut self) {
        for (tx, held) in std::mem::take(&mut self.unconfirmed) {
            answer(tx, held);
        }
    }

    /// Answers requests validation decided without writing whose answers had to wait for the
    /// batch (`validate`). An answer decided on a change earlier in the batch stands only if
    /// that change was written: otherwise nothing was, and the request is to be retried. Any
    /// answer decided while changes are unconfirmed rests on them, as a delete of a chunk an
    /// unconfirmed delete removed does, or a refusal of a put that differs from an
    /// unconfirmed one, and is held with them.
    fn settle(&mut self, answered: Vec<Answered>, written: &[bool]) {
        for Answered {
            reply,
            result,
            leader,
        } in answered
        {
            let result = if self.shared.fenced.load(Ordering::Acquire) {
                Err(ChunkError::Fenced)
            } else if leader.is_some_and(|i| !written.get(i).copied().unwrap_or(false)) {
                Err(ChunkError::Busy)
            } else {
                result
            };
            if self.unconfirmed.is_empty() || matches!(result, Err(ChunkError::Fenced)) {
                answer(reply, result);
            } else {
                self.unconfirmed.push((reply, result));
            }
        }
    }

    /// Whether cleaning may still free a segment, and the cleaner's verdict that says so: unless
    /// it was tried on exactly the data deleted so far and gained nothing. Only trying can tell,
    /// since how tightly relocated records pack is what decides.
    fn reclaimable(&self) -> Option<u64> {
        let verdict = self.shared.futile_at.load(Ordering::Relaxed);
        if verdict == self.shared.dead_bytes.load(Ordering::Relaxed) {
            None
        } else {
            Some(verdict)
        }
    }

    fn free_segments(&self) -> usize {
        self.segments.free_count()
    }

    fn poke_cleaner(&self) {
        if self.free_segments() < self.shared.low_water.load(Ordering::Relaxed)
            && let Some(poke) = &self.poke
        {
            // A poke already waiting is as good as a second one.
            let _ = poke.try_send(());
        }
    }

    fn process(&mut self, batch: Vec<Request>) {
        let (checkpoints, batch): (Vec<Request>, Vec<Request>) = batch
            .into_iter()
            .partition(|r| matches!(r.op, Op::Checkpoint));
        if self.checkpoint_due(batch.len())
            && let Err(e) = self.checkpoint()
        {
            self.fence();
            let mut first = Some(e);
            for request in batch.into_iter().chain(checkpoints) {
                reply(request, Err(first.take().unwrap_or(ChunkError::Fenced)));
            }
            return;
        }
        self.handle(batch);
        if checkpoints.is_empty() {
            return;
        }
        let result = if self.shared.fenced.load(Ordering::Acquire) {
            Err(ChunkError::Fenced)
        } else {
            self.checkpoint()
        };
        match result {
            Ok(()) => {
                for request in checkpoints {
                    reply(request, Ok(()));
                }
            }
            Err(e) => {
                self.fence();
                let mut first = Some(e);
                for request in checkpoints {
                    reply(request, Err(first.take().unwrap_or(ChunkError::Fenced)));
                }
            }
        }
    }

    /// Whether the log must be checkpointed before a batch of `requests` requests: after the
    /// live log, the batch's frame, then a wrap and a checkpoint of the index as the batch may
    /// leave it must fit, or the log would fill with nothing able to free it. Waiting until then
    /// leaves at least `3·C_max − 2·C` of frames between checkpoints of size `C` (the log's
    /// sizing, layout.rs), so a checkpoint takes at most half the log's writes, at the full
    /// budget, and far less below it; and replay never exceeds one log (docs/research/11 §9.3).
    fn checkpoint_due(&self, requests: usize) -> bool {
        let geometry = &self.shared.geometry;
        let reserve = || {
            let checkpoint = checkpoint_bytes(
                self.fragments.checked_add(u64::try_from(requests).ok()?)?,
                u64::from(geometry.segments),
                geometry.block,
            )?;
            let requests = self.config.limits.batch_requests;
            let batch = batch_frame_bytes(requests, requests, geometry.block)?;
            let wrap = largest_frame(&self.config, u64::from(geometry.segments), geometry.block)?;
            checkpoint
                .checked_add(batch)?
                .checked_add(wrap)?
                .checked_add(self.cursor.used)
        };
        reserve().is_none_or(|need| need > geometry.log_size)
    }

    /// Validates and commits the batch's writes, deletes and relocations.
    fn handle(&mut self, batch: Vec<Request>) {
        let (accepted, answered) = match self.validate(batch) {
            Ok(validated) => validated,
            Err(()) => return,
        };
        let reclaimable = self.segments.reclaimable().next().is_some();
        let written = if accepted.is_empty() && !reclaimable {
            Vec::new()
        } else {
            self.commit(accepted)
        };
        self.settle(answered, &written);
    }

    /// Reads the durable fragment `f` of `key` and compares its bytes with `bytes`.
    fn stored_bytes(
        shared: &Shared<F>,
        reads: &mut ReadPool,
        key: &ChunkKey,
        f: &Fragment,
        bytes: &[u8],
    ) -> Stored {
        let identity = Identity {
            volume: shared.volume,
            checksum_shift: shared.checksum_shift,
        };
        let mut data = Vec::new();
        let read = verify_at(
            &shared.file,
            reads,
            &shared.geometry,
            identity,
            f.segment,
            u64::from(f.offset),
            Some(&mut data),
        );
        match read {
            Ok(Some(v)) if v.is(key, f) => {
                if data == bytes {
                    Stored::Same
                } else {
                    Stored::Differs
                }
            }
            _ => Stored::Damaged,
        }
    }

    /// Answers requests that need no write, refuses invalid ones, and returns the rest, with
    /// the answers that must wait for the batch: those decided on a change earlier in it, and
    /// any decided while the last batch is unconfirmed (`settle`). The rest rest on the index
    /// alone, durable and confirmed, and go out at once.
    fn validate(&mut self, batch: Vec<Request>) -> Result<(Vec<Accepted>, Vec<Answered>), ()> {
        let index = match self.shared.index.read() {
            Ok(index) => index,
            Err(_) => {
                self.fence();
                for request in batch {
                    reply(request, Err(ChunkError::Fenced));
                }
                return Err(());
            }
        };
        let limits = self.config.limits;
        let max_payload = max_payload(&self.shared.geometry, self.shared.checksum_shift);
        let mut views: HashMap<ChunkKey, Option<View>> = HashMap::new();
        let mut added: u64 = 0;
        let mut accepted = Vec::with_capacity(batch.len());
        let mut answered = Vec::new();
        // The position in `accepted` of the last write or delete of each chunk.
        let mut changed_by: HashMap<ChunkKey, usize> = HashMap::new();
        let view_of = |views: &mut HashMap<ChunkKey, Option<View>>, key: &ChunkKey| {
            views
                .entry(*key)
                .or_insert_with(|| {
                    index.get(key).map(|e| View {
                        len: e.len(),
                        sealed: e.sealed,
                        fragments: e.fragments.len(),
                        pending: Vec::new(),
                        touched: false,
                    })
                })
                .clone()
        };
        for request in batch {
            let key = match &request.op {
                Op::Write { key, .. } | Op::Delete { key } => Some(*key),
                Op::Relocate { .. } | Op::Checkpoint | Op::Free => None,
            };
            let decision = match &request.op {
                Op::Write {
                    key,
                    offset,
                    payload,
                    seal,
                } => {
                    // Appending nothing without sealing writes nothing. A zero-length fragment
                    // may only end a chunk: fragments are found by their starting offset, which
                    // must be unique.
                    if payload.data.is_empty() && !*seal {
                        reply(request, Ok(()));
                        continue;
                    }
                    let view = view_of(&mut views, key);
                    let len = u64::try_from(payload.data.len()).unwrap_or(u64::MAX);
                    let crc = payload.crc;
                    let stored = index.get(key).and_then(|e| e.fragment_at(*offset)).copied();
                    let existing = stored
                        .map(|f| (u64::from(f.payload_len), f.payload_crc))
                        .or_else(|| {
                            view.as_ref().and_then(|v| {
                                v.pending
                                    .iter()
                                    .find(|(o, _, _)| o == offset)
                                    .map(|(_, l, c)| (*l, *c))
                            })
                        });
                    match view {
                        Some(v) if *offset < v.len || (v.sealed && *offset == v.len) => {
                            let refused = if *offset == 0 && *seal {
                                ChunkError::Exists(*key)
                            } else if v.sealed {
                                ChunkError::Sealed(*key)
                            } else {
                                ChunkError::Conflict {
                                    key: *key,
                                    offset: *offset,
                                }
                            };
                            if existing != Some((len, crc)) || (*seal && !v.sealed) {
                                Err(refused)
                            } else {
                                // The same length and CRC-32C, which a different payload can
                                // share: the bytes decide whether this is a retry (audit B05).
                                match stored {
                                    Some(f) => match Self::stored_bytes(
                                        &self.shared,
                                        &mut self.reads,
                                        key,
                                        &f,
                                        &payload.data,
                                    ) {
                                        Stored::Same => Ok(Decision::Done),
                                        Stored::Differs => Err(refused),
                                        // The stored copy no longer verifies: the retry, the
                                        // same length and CRC as the bytes acknowledged,
                                        // writes it again (audit S05).
                                        Stored::Damaged => Ok(Decision::Write(crc)),
                                    },
                                    None => {
                                        let same = accepted.iter().rev().any(|a: &Accepted| {
                                            matches!(
                                                &a.request.op,
                                                Op::Write { key: k, offset: o, payload: p, .. }
                                                    if k == key && o == offset && p.data == payload.data
                                            )
                                        });
                                        if same {
                                            Ok(Decision::Done)
                                        } else {
                                            Err(refused)
                                        }
                                    }
                                }
                            }
                        }
                        Some(v) if *offset > v.len => Err(ChunkError::Gap {
                            key: *key,
                            offset: *offset,
                            end: v.len,
                        }),
                        None if *offset > 0 => Err(ChunkError::Gap {
                            key: *key,
                            offset: *offset,
                            end: 0,
                        }),
                        v => {
                            let fragments = v.as_ref().map_or(0, |v| v.fragments);
                            if len > max_payload {
                                Err(ChunkError::TooLarge {
                                    len,
                                    max: max_payload,
                                })
                            } else if fragments >= limits.fragments_per_chunk {
                                Err(ChunkError::TooManyFragments(*key))
                            } else if self.fragments.saturating_add(added)
                                >= self.config.max_fragments
                            {
                                Err(ChunkError::Full)
                            } else {
                                added = added.saturating_add(1);
                                let mut next = v.unwrap_or(View {
                                    len: 0,
                                    sealed: false,
                                    fragments: 0,
                                    pending: Vec::new(),
                                    touched: false,
                                });
                                next.pending.push((*offset, len, crc));
                                next.len = next.len.saturating_add(len);
                                next.sealed = *seal;
                                next.fragments = next.fragments.saturating_add(1);
                                next.touched = true;
                                views.insert(*key, Some(next));
                                Ok(Decision::Write(crc))
                            }
                        }
                    }
                }
                Op::Delete { key } => {
                    if view_of(&mut views, key).is_some() {
                        views.insert(*key, None);
                        Ok(Decision::Delete)
                    } else {
                        Ok(Decision::Done)
                    }
                }
                Op::Relocate { moves } => {
                    let current: Vec<usize> = moves
                        .iter()
                        .enumerate()
                        .filter(|(_, m)| {
                            let now = index
                                .get(&m.key)
                                .and_then(|e| e.fragment_at(m.from.chunk_offset));
                            let untouched = views
                                .get(&m.key)
                                .is_none_or(|v| v.as_ref().is_some_and(|v| !v.touched));
                            now == Some(&m.from) && untouched && m.payload.crc == m.from.payload_crc
                        })
                        .map(|(i, _)| i)
                        .collect();
                    if current.is_empty() {
                        Ok(Decision::Done)
                    } else {
                        Ok(Decision::Relocate(current))
                    }
                }
                Op::Free => Ok(Decision::Free),
                // Taken out of the batch before validation (`process`).
                Op::Checkpoint => Ok(Decision::Done),
            };
            let result = match decision {
                Ok(Decision::Done) => Ok(()),
                Ok(decision) => {
                    if let (Some(key), Decision::Write(_) | Decision::Delete) = (key, &decision) {
                        changed_by.insert(key, accepted.len());
                    }
                    accepted.push(Accepted { request, decision });
                    continue;
                }
                Err(e) => Err(e),
            };
            let leader = key.and_then(|k| changed_by.get(&k).copied());
            if leader.is_none() && self.unconfirmed.is_empty() {
                reply(request, result);
            } else {
                answered.push(Answered {
                    reply: request.reply,
                    result,
                    leader,
                });
            }
        }
        Ok((accepted, answered))
    }

    /// Where a batch of `requests` requests placing at most `records` records can go.
    fn layout(&self, requests: usize, records: usize) -> Layout {
        // Sealed segments with nothing live are freed in this frame, so this batch may reuse
        // them; at most one per request keeps the frame within its bound
        // (layout::batch_frame_payload).
        let freed: Vec<u32> = self.segments.reclaimable().take(requests.max(1)).collect();
        // Incarnation and write position, before this batch, of each segment it may seal or
        // free.
        let mut before = HashMap::new();
        for &segment in self.opens.iter().flatten().chain(&freed) {
            if let Some(s) = self.segments.get(segment)
                && s.state != SegmentState::Free
            {
                before.insert(segment, (s.incarnation, u64::from(s.write_pos)));
            }
        }
        // A stream whose open segment is freed opens a new one when it next writes.
        let mut opens = self.opens;
        for open in &mut opens {
            if open.is_some_and(|s| freed.contains(&s)) {
                *open = None;
            }
        }
        // Each record opens at most one segment (`Layout::place`), so the lowest `records`
        // of the free segments and those freed in this frame are all the batch can take.
        let mut free: Vec<u32> = self
            .segments
            .free()
            .take(records)
            .chain(freed.iter().copied())
            .collect();
        // Lowest segment first: popped from the end.
        free.sort_unstable_by(|a, b| b.cmp(a));
        let free_left = self.segments.free_count().saturating_add(freed.len());
        let mut records = Vec::new();
        for &segment in &freed {
            if let Some(&(incarnation, _)) = before.get(&segment) {
                records.push(LogRecord::Segment(SegmentRecord {
                    segment,
                    incarnation,
                    state: SegmentState::Free,
                    write_pos: 0,
                }));
            }
        }
        Layout {
            geometry: self.shared.geometry,
            volume: self.shared.volume,
            now: now_ns(),
            records,
            regions: Vec::new(),
            positions: HashMap::new(),
            opened: Vec::new(),
            clean_opened: 0,
            sealed: Vec::new(),
            freed,
            opens,
            incarnation: self.incarnation,
            free,
            free_left,
            before,
        }
    }

    /// Lays out, writes, flushes, publishes and answers one batch, and says which of the
    /// accepted requests it wrote.
    fn commit(&mut self, accepted: Vec<Accepted>) -> Vec<bool> {
        let records = accepted
            .iter()
            .map(|a| match &a.decision {
                Decision::Write(_) => 1,
                Decision::Relocate(current) => current.len(),
                _ => 0,
            })
            .fold(0usize, usize::saturating_add);
        let mut layout = self.layout(accepted.len(), records);
        let mut sequence = self.sequence;
        let mut ok: Vec<Reply> = Vec::with_capacity(accepted.len());
        // Answered once a later frame confirms this batch's (`confirm`): deletes, which live
        // only in the frame, and puts in segments the batch opens, which recovery finds only
        // through the frame's record of the opening, since it rolls forward through open
        // segments alone.
        let mut held: Vec<Reply> = Vec::new();
        let mut written = vec![false; accepted.len()];
        let mut payloads: Vec<Payload> = Vec::with_capacity(accepted.len());
        let shift = self.shared.checksum_shift;
        for (i, Accepted { request, decision }) in accepted.into_iter().enumerate() {
            let mut wrote = || {
                if let Some(w) = written.get_mut(i) {
                    *w = true;
                }
            };
            let Request {
                op,
                reply: tx,
                queued,
            } = request;
            match (op, decision) {
                (
                    Op::Write {
                        key,
                        offset,
                        payload,
                        seal,
                    },
                    Decision::Write(crc),
                ) => {
                    sequence = sequence.saturating_add(1);
                    let flags = if seal { FLAG_FINAL } else { 0 };
                    let put = Put {
                        key,
                        chunk_offset: offset,
                        flags,
                        sequence,
                        time_ns: layout.now,
                        crc,
                    };
                    payloads.push(payload);
                    let index = payloads.len().saturating_sub(1);
                    match place_record(&mut layout, Stream::Client, shift, &put, &payloads, index) {
                        Ok(record) => {
                            let opened = layout.opened.iter().any(|&(s, _)| s == record.segment);
                            layout.records.push(LogRecord::Put(record));
                            wrote();
                            if opened {
                                held.push(tx);
                            } else {
                                ok.push(tx);
                            }
                        }
                        // No free segment for the client while cleaning may free one: the
                        // put waits for the cleaner's verdict, written once a segment is freed
                        // or refused `Full` for good, so its answer rests on the volume and not
                        // on how soon a client asks again (research/75). With no cleaner to
                        // give a verdict it is to be retried.
                        // The verdict the check read is the one the write is held against
                        // (`Waiting`).
                        Err(ChunkError::Full) => match self.reclaimable() {
                            Some(verdict) => match (payloads.pop(), self.poke.is_some()) {
                                (Some(payload), true) => self.wait(
                                    Request {
                                        op: Op::Write {
                                            key,
                                            offset,
                                            payload,
                                            seal,
                                        },
                                        reply: tx,
                                        queued,
                                    },
                                    verdict,
                                ),
                                _ => answer(tx, Err(ChunkError::Busy)),
                            },
                            None => answer(tx, Err(ChunkError::Full)),
                        },
                        Err(e) => answer(tx, Err(e)),
                    }
                }
                (Op::Relocate { moves }, Decision::Relocate(current)) => {
                    let mut result = Ok(());
                    for (i, m) in moves.into_iter().enumerate() {
                        if !current.contains(&i) {
                            continue;
                        }
                        let put = Put {
                            key: m.key,
                            chunk_offset: m.from.chunk_offset,
                            flags: m.flags,
                            sequence: m.from.sequence,
                            time_ns: m.time_ns,
                            crc: m.from.payload_crc,
                        };
                        payloads.push(m.payload);
                        let index = payloads.len().saturating_sub(1);
                        match place_record(
                            &mut layout,
                            Stream::Clean,
                            shift,
                            &put,
                            &payloads,
                            index,
                        ) {
                            Ok(record) => layout.records.push(LogRecord::Put(record)),
                            Err(e) => {
                                result = Err(e);
                                break;
                            }
                        }
                    }
                    match result {
                        Ok(()) => {
                            wrote();
                            ok.push(tx);
                        }
                        Err(e) => answer(tx, Err(e)),
                    }
                }
                (Op::Delete { key }, Decision::Delete) => {
                    sequence = sequence.saturating_add(1);
                    layout.records.push(LogRecord::Delete(DeleteRecord {
                        key,
                        sequence,
                        time_ns: layout.now,
                    }));
                    wrote();
                    held.push(tx);
                }
                (Op::Free, Decision::Free) => ok.push(tx),
                _ => {
                    self.fence();
                    answer(
                        tx,
                        Err(ChunkError::Internal("a request and its decision disagree")),
                    );
                }
            }
        }
        // Nothing placed, sealed or freed: no frame, and a free has nothing to wait for.
        if layout.records.is_empty() {
            for tx in ok {
                answer(tx, Ok(()));
            }
            return written;
        }

        let result = self
            .reserve(sequence, layout.incarnation)
            .and_then(|()| self.write_batch(&layout, &payloads))
            .and_then(|()| self.publish(&layout));
        match result {
            Err(e) => {
                self.fence();
                let mut first = Some(e);
                for tx in ok.into_iter().chain(held) {
                    answer(tx, Err(first.take().unwrap_or(ChunkError::Fenced)));
                }
                vec![false; written.len()]
            }
            Ok(()) => {
                self.incarnation = layout.incarnation;
                self.sequence = sequence;
                self.shared
                    .clean_opened
                    .fetch_add(layout.clean_opened, Ordering::Relaxed);
                for tx in ok {
                    answer(tx, Ok(()));
                }
                // This batch's frame, durable, confirms the last one's.
                self.confirmed();
                self.unconfirmed = held.into_iter().map(|tx| (tx, Ok(()))).collect();
                written
            }
        }
    }

    /// Raises the superblock's reservations, durably, if `sequence` or `incarnation` is past
    /// them; runs before any record carrying either is written.
    fn reserve(&mut self, sequence: u64, incarnation: u64) -> Result<(), ChunkError> {
        if sequence <= self.superblock.sequence_limit
            && incarnation <= self.superblock.incarnation_limit
        {
            return Ok(());
        }
        let mut superblock = self.superblock.clone();
        superblock.sequence = superblock.sequence.saturating_add(1);
        superblock.sequence_limit = superblock
            .sequence_limit
            .max(sequence.saturating_add(SEQUENCE_RESERVE));
        superblock.incarnation_limit = superblock
            .incarnation_limit
            .max(incarnation.saturating_add(INCARNATION_RESERVE));
        let align = self.shared.file.alignment();
        write_superblocks(&mut self.io, align, &[&superblock])?;
        self.superblock = superblock;
        Ok(())
    }

    /// Encodes each region straight into an aligned buffer of its size and the batch's index
    /// frame after them, hands them all at once to the device's issuer, and flushes once when
    /// every one has completed. The flush makes every one durable, and until it does they reach
    /// the device in any order however they are issued, so issuing them together changes
    /// nothing recovery sees. The frame written after the records made a 32 MiB batch 1.9 ms
    /// slower, and written beside them nothing (docs/measurements/2026-09-29-frame-overlap.md).
    /// The issuer's workers carry them, as deep as the device's measured depth; no thread
    /// starts for a region.
    fn write_batch(&mut self, layout: &Layout, payloads: &[Payload]) -> Result<(), ChunkError> {
        let started = std::time::Instant::now();
        let geometry = self.shared.geometry;
        let block = geometry.block_usize();
        let group = self.cursor.lsn;
        let placed = self.place_frame(KIND_BATCH, &layout.records, group)?;
        let frames = self.frame_writes(placed)?;
        let mut writes = Vec::with_capacity(layout.regions.len().saturating_add(frames.len()));
        for region in &layout.regions {
            let len = usize::try_from(region.end.saturating_sub(region.start))
                .map_err(|_| ChunkError::Full)?;
            let padded = len
                .checked_next_multiple_of(block)
                .ok_or(ChunkError::Full)?;
            let mut buf = self
                .pool
                .take(padded)
                .map_err(|e| ChunkError::Device(e.into()))?;
            for part in &region.parts {
                let encoded = match part {
                    Part::Segment(header) => buf.put(&header.encode(block)),
                    Part::Record { header, payload } => payloads
                        .get(*payload)
                        .and_then(|p| record::encode(header, &p.table, &p.data, &mut buf)),
                };
                encoded.ok_or(ChunkError::Internal("a record did not encode as placed"))?;
            }
            if buf.len() != len {
                return Err(ChunkError::Internal(
                    "a region's records did not fill it as placed",
                ));
            }
            buf.extend_zeros(padded.saturating_sub(len))
                .map_err(|e| ChunkError::Device(e.into()))?;
            let at = geometry
                .segment_offset(region.segment)
                .and_then(|o| o.checked_add(region.start))
                .ok_or(ChunkError::Full)?;
            writes.push((buf, at));
        }
        writes.extend(frames);
        let written = self.io.write(writes, true).map_err(ChunkError::Device)?;
        for (buf, _) in written {
            self.pool.give(buf);
        }
        let took = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.anticipation.served(took);
        let laid: u64 = payloads
            .iter()
            .map(|p| u64::try_from(p.data.len()).unwrap_or(u64::MAX))
            .fold(0, u64::saturating_add);
        let rate = u128::from(laid)
            .saturating_mul(1_000_000_000)
            .checked_div(u128::from(took))
            .and_then(|r| u64::try_from(r).ok())
            .unwrap_or(0);
        self.shared.peak_rate.fetch_max(rate, Ordering::Relaxed);
        self.shared
            .service_ns
            .store(self.anticipation.service_ns(), Ordering::Relaxed);
        Ok(())
    }

    /// Appends one frame to the log (preceded by a wrap frame when it must go around), and
    /// flushes when `flush` is set. `group` is the LSN of the first frame written since the
    /// last completed flush.
    fn append_frame(
        &mut self,
        kind: u16,
        records: &[LogRecord],
        group: u64,
        flush: bool,
    ) -> Result<(u64, u64), ChunkError> {
        let placed = self.place_frame(kind, records, group)?;
        let (at, lsn) = (placed.at, placed.lsn);
        let writes = self.frame_writes(placed)?;
        for (buf, _) in self.io.write(writes, flush).map_err(ChunkError::Device)? {
            self.pool.give(buf);
        }
        Ok((at, lsn))
    }

    /// A placed frame's writes: each encoded frame in an aligned buffer from the writer's
    /// pool, at its offset in the file. The buffers come back to the pool once written, as the
    /// regions' do, so the pool holds frames' buffers for frames and never fills with them.
    fn frame_writes(&mut self, placed: Placed) -> Result<Vec<(AlignedBuf, u64)>, ChunkError> {
        let geometry = self.shared.geometry;
        placed
            .writes
            .into_iter()
            .map(|(pos, encoded)| {
                let mut buf = self
                    .pool
                    .take(encoded.len())
                    .map_err(|e| ChunkError::Device(e.into()))?;
                buf.extend_from_slice(&encoded)
                    .map_err(|e| ChunkError::Device(e.into()))?;
                Ok((buf, geometry.log_offset.saturating_add(pos)))
            })
            .collect()
    }

    /// Places one frame in the log, preceded by a wrap frame when it must go around, and
    /// moves the log's cursor past it. A write that then fails fences the volume, so the
    /// cursor is not read again before recovery.
    fn place_frame(
        &mut self,
        kind: u16,
        records: &[LogRecord],
        group: u64,
    ) -> Result<Placed, ChunkError> {
        let geometry = self.shared.geometry;
        let block = geometry.block_usize();
        let probe = frame::encode(kind, 0, 0, self.shared.volume, records, block)
            .ok_or(ChunkError::Full)?;
        let len = u64::try_from(probe.len()).map_err(|_| ChunkError::Full)?;
        // Recovery reads no larger frame, and would take this one for the log's end, losing
        // what it holds and everything after it (audit S13). The settings checked at format
        // and open keep batches within it and checkpoints pack their records into it, so
        // reaching this is a fault in that accounting: the batch fails, and nothing is lost.
        if len > MAX_FRAME_BYTES {
            return Err(ChunkError::Internal(
                "an index frame larger than recovery reads",
            ));
        }
        let placement = self
            .cursor
            .place(len, geometry.block, geometry.log_size)
            .ok_or(ChunkError::Full)?;
        let mut lsn = self.cursor.lsn;
        let mut writes = Vec::with_capacity(2);
        if placement.wrap {
            let wrap = frame::encode(KIND_WRAP, lsn, group, self.shared.volume, &[], block)
                .ok_or(ChunkError::Full)?;
            writes.push((self.cursor.pos, wrap));
            lsn = lsn.saturating_add(1);
        }
        let bytes = frame::encode(kind, lsn, group, self.shared.volume, records, block)
            .ok_or(ChunkError::Full)?;
        writes.push((placement.at, bytes));
        self.cursor.advance(placement, len, geometry.log_size);
        Ok(Placed {
            writes,
            at: placement.at,
            lsn,
        })
    }

    /// Applies a durable batch to the index and segment table, where readers see it.
    fn publish(&mut self, layout: &Layout) -> Result<(), ChunkError> {
        let per_chunk = self.config.limits.fragments_per_chunk;
        let block = self.shared.geometry.block;
        let mut index = self.shared.index.write().map_err(|_| ChunkError::Fenced)?;
        for &segment in &layout.freed {
            self.segments
                .update(segment, |info| *info = SegmentInfo::FREE);
        }
        for &(segment, incarnation) in &layout.opened {
            self.segments.update(segment, |info| {
                *info = SegmentInfo {
                    state: SegmentState::Open,
                    incarnation,
                    write_pos: u32::try_from(block).unwrap_or(u32::MAX),
                    live: 0,
                    youngest_ns: 0,
                };
            });
        }
        for record in &layout.records {
            match record {
                LogRecord::Put(p) => {
                    let outcome =
                        index
                            .insert(p.key, p, per_chunk)
                            .map_err(|_| ChunkError::CorruptLog {
                                lsn: self.cursor.lsn,
                            })?;
                    let credit = match outcome {
                        Inserted::Added => {
                            self.fragments = self.fragments.saturating_add(1);
                            true
                        }
                        // A relocated copy: the old one dies as an equal one is born, so no
                        // space is freed that cleaning could gain.
                        Inserted::Replaced(old) => {
                            self.segments.update(old.segment, |info| {
                                info.live = info.live.saturating_sub(u64::from(old.record_len));
                            });
                            true
                        }
                        Inserted::Unchanged => false,
                    };
                    if credit {
                        self.segments.update(p.segment, |info| {
                            info.live = info.live.saturating_add(u64::from(p.record_len));
                            info.youngest_ns = info.youngest_ns.max(p.time_ns);
                        });
                    }
                }
                LogRecord::Delete(d) => {
                    if let Some(entry) = index.remove(&d.key) {
                        for f in &entry.fragments {
                            self.fragments = self.fragments.saturating_sub(1);
                            self.segments.update(f.segment, |info| {
                                info.live = info.live.saturating_sub(u64::from(f.record_len));
                            });
                            self.shared
                                .dead_bytes
                                .fetch_add(u64::from(f.record_len), Ordering::Relaxed);
                        }
                    }
                }
                LogRecord::Segment(_) => {}
            }
        }
        drop(index);
        for region in &layout.regions {
            let end = region
                .end
                .checked_next_multiple_of(block)
                .unwrap_or(region.end);
            let end = u32::try_from(end).unwrap_or(u32::MAX);
            self.segments.update(region.segment, |info| {
                info.write_pos = info.write_pos.max(end);
            });
        }
        for &(segment, pos) in &layout.sealed {
            self.segments.update(segment, |info| {
                info.state = SegmentState::Sealed;
                info.write_pos = info.write_pos.max(pos);
            });
        }
        self.opens = layout.opens;
        self.shared
            .clean_open
            .store(open_at(self.opens, Stream::Clean), Ordering::Relaxed);
        // Readers get the segments this batch changed, not a copy of the table.
        let changed = self.segments.take_changed();
        if let Ok(mut usage) = self.shared.usage.write() {
            usage.copy(&self.segments, &changed);
        }
        Ok(())
    }

    /// Writes the whole index into the log and points the superblock at it, freeing the log
    /// before it (docs/design/chunk-store.md §5). The records, each segment's state and then
    /// each fragment, are packed into frames of at most `MAX_FRAME_BYTES`, the first a
    /// checkpoint's beginning, the rest its chunks; recovery replays them as it replays any
    /// frame, so a checkpoint of any size is read back whole (audit S13). The fragments are
    /// read from the index as they are written, not gathered first.
    pub(crate) fn checkpoint(&mut self) -> Result<(), ChunkError> {
        let segment_records: Vec<LogRecord> = self
            .segments
            .iter()
            .filter(|(_, s)| s.state != SegmentState::Free)
            .map(|(segment, s)| {
                LogRecord::Segment(SegmentRecord {
                    segment,
                    incarnation: s.incarnation,
                    state: s.state,
                    write_pos: s.write_pos,
                })
            })
            .collect();
        // Only this thread changes the index, so holding it to read while the frames are
        // written keeps no writer waiting.
        let shared = Arc::clone(&self.shared);
        let index = shared.index.read().map_err(|_| ChunkError::Fenced)?;
        let puts = index.iter().flat_map(|(key, entry)| {
            let last = entry.fragments.len().saturating_sub(1);
            entry.fragments.iter().enumerate().map(move |(i, f)| {
                let flags = if entry.sealed && i == last {
                    FLAG_FINAL
                } else {
                    0
                };
                put_of(*key, f, flags, entry.time_ns)
            })
        });
        // Every frame of the checkpoint shares one flush.
        let group = self.cursor.lsn;
        let mut begin = None;
        pack(segment_records.into_iter().chain(puts), |frame| {
            let kind = if begin.is_none() {
                KIND_CHECKPOINT_BEGIN
            } else {
                KIND_CHECKPOINT_CHUNK
            };
            let at = self.append_frame(kind, frame, group, false)?;
            begin.get_or_insert(at);
            Ok(())
        })?;
        drop(index);
        let (begin_pos, begin_lsn) =
            begin.ok_or(ChunkError::Internal("a checkpoint without its first frame"))?;
        self.append_frame(KIND_CHECKPOINT_END, &[], group, true)?;

        let mut superblock = self.superblock.clone();
        superblock.sequence = superblock.sequence.saturating_add(1);
        superblock.start_lsn = begin_lsn;
        superblock.start_pos = begin_pos;
        superblock.end_lsn = self.cursor.lsn;
        let align = self.shared.file.alignment();
        write_superblocks(&mut self.io, align, &[&superblock])?;
        self.superblock = superblock;

        // The log now starts at the checkpoint.
        self.cursor.start_pos = begin_pos;
        self.cursor.start_lsn = begin_lsn;
        self.cursor.used = distance(begin_pos, self.cursor.pos, self.shared.geometry.log_size);
        self.shared.checkpoints.fetch_add(1, Ordering::Relaxed);
        // The checkpoint's frames, durable, follow the last batch's.
        self.confirmed();
        Ok(())
    }
}

/// The largest payload one record can hold: a segment less its header block
/// (`layout::record_room`).
pub(crate) fn max_payload(geometry: &Geometry, checksum_shift: u8) -> u64 {
    crate::layout::record_room(geometry.segment_size, geometry.block, checksum_shift)
}

/// Packs `records`, in order, into frames whose records fill at most `FRAME_PAYLOAD` bytes,
/// a frame closing only when the next record does not fit, and hands each to `write`. It
/// writes at least one frame, empty when there are no records.
fn pack<E>(
    records: impl Iterator<Item = LogRecord>,
    mut write: impl FnMut(&[LogRecord]) -> Result<(), E>,
) -> Result<(), E> {
    let mut frame: Vec<LogRecord> = Vec::new();
    let mut bytes = 0u64;
    let mut written = false;
    for record in records {
        let len = record.encoded_len() as u64;
        if !frame.is_empty() && bytes.saturating_add(len) > FRAME_PAYLOAD {
            write(&frame)?;
            written = true;
            frame.clear();
            bytes = 0;
        }
        bytes = bytes.saturating_add(len);
        frame.push(record);
    }
    if !frame.is_empty() || !written {
        write(&frame)?;
    }
    Ok(())
}

/// What a record says about the fragment it holds, apart from its placement.
struct Put {
    key: ChunkKey,
    chunk_offset: u64,
    flags: u8,
    sequence: u64,
    time_ns: u64,
    crc: u32,
}

/// Places the record for `put`, whose payload is `payloads[payload]`, in `stream` and
/// returns its Put record. The record is encoded when the batch is written.
fn place_record(
    layout: &mut Layout,
    stream: Stream,
    shift: u8,
    put: &Put,
    payloads: &[Payload],
    payload: usize,
) -> Result<PutRecord, ChunkError> {
    let data_len = payloads
        .get(payload)
        .map(|p| p.data.len())
        .ok_or(ChunkError::Internal("a payload went missing"))?;
    let payload_len = u32::try_from(data_len).map_err(|_| ChunkError::Full)?;
    let len = record::record_len(payload_len, shift)
        .and_then(|l| u64::try_from(l).ok())
        .ok_or(ChunkError::Full)?;
    // A new write's record goes to the stream its length takes; a relocation keeps the cleaner's.
    let stream = match stream {
        Stream::Clean => Stream::Clean,
        Stream::Client | Stream::Whole => Stream::of_write(len, layout.geometry.segment_size),
    };
    let (segment, at) = layout.place(stream, len).ok_or(ChunkError::Full)?;
    let header = RecordHeader {
        flags: put.flags,
        checksum_shift: shift,
        volume: layout.volume,
        segment,
        incarnation: layout.incarnation_of(segment),
        sequence: put.sequence,
        key: put.key,
        chunk_offset: put.chunk_offset,
        payload_len,
        time_ns: put.time_ns,
    };
    let incarnation = header.incarnation;
    layout.stage(segment, at, len, header, payload);
    Ok(PutRecord {
        key: put.key,
        segment,
        incarnation,
        offset: u32::try_from(at).map_err(|_| ChunkError::Full)?,
        record_len: u32::try_from(len).map_err(|_| ChunkError::Full)?,
        chunk_offset: put.chunk_offset,
        payload_len,
        payload_crc: put.crc,
        sequence: put.sequence,
        time_ns: put.time_ns,
        flags: put.flags,
    })
}

/// The segment `stream` has open among `opens`, as `Shared::clean_open` holds it.
pub(crate) fn open_at(opens: [Option<u32>; 3], stream: Stream) -> u64 {
    opens
        .get(stream as usize)
        .copied()
        .flatten()
        .map_or(u64::MAX, u64::from)
}

/// Bytes from `from` forward to `to` in a circular region of `size` bytes.
pub(crate) fn distance(from: u64, to: u64, size: u64) -> u64 {
    if to >= from {
        to.saturating_sub(from)
    } else {
        size.saturating_sub(from).saturating_add(to)
    }
}

pub(crate) fn put_of(key: ChunkKey, f: &Fragment, flags: u8, time_ns: u64) -> LogRecord {
    LogRecord::Put(PutRecord {
        key,
        segment: f.segment,
        incarnation: f.incarnation,
        offset: f.offset,
        record_len: f.record_len,
        chunk_offset: f.chunk_offset,
        payload_len: f.payload_len,
        payload_crc: f.payload_crc,
        sequence: f.sequence,
        time_ns,
        flags,
    })
}

/// A frame placed in the log.
struct Placed {
    /// Each frame to write, with its position in the log: a wrap frame first when the log
    /// goes around, then the frame itself.
    writes: Vec<(u64, Vec<u8>)>,
    /// The frame's position and LSN.
    at: u64,
    lsn: u64,
}

/// Writes each superblock to the copy its sequence selects, through the device's issuer, then
/// flushes.
pub(crate) fn write_superblocks(
    io: &mut Attached,
    align: Alignment,
    superblocks: &[&Superblock],
) -> Result<(), ChunkError> {
    let mut writes = Vec::with_capacity(superblocks.len());
    for sb in superblocks {
        let bytes = sb.encode();
        let mut buf =
            AlignedBuf::zeroed(bytes.len(), align).map_err(|e| ChunkError::Device(e.into()))?;
        buf.extend_from_slice(&bytes)
            .map_err(|e| ChunkError::Device(e.into()))?;
        writes.push((buf, sb.offset_of(sb.slot())));
    }
    io.write(writes, true).map_err(ChunkError::Device)?;
    Ok(())
}

fn request_bytes(request: &Request) -> usize {
    match &request.op {
        Op::Write { payload, .. } => payload.data.len(),
        Op::Relocate { moves } => moves.iter().map(|m| m.payload.data.len()).sum(),
        Op::Delete { .. } | Op::Checkpoint | Op::Free => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Limits;

    /// A checkpoint of any size packs into frames recovery reads (audit S13): 300,000
    /// segments' states, more than one 4 MiB frame holds, then 60,000 fragments. Every frame
    /// fits `MAX_FRAME_BYTES` once encoded, a frame closes only when the next record does not
    /// fit, the records keep their order, the space they take is within what the log was
    /// sized for, and an empty checkpoint still writes its first frame.
    #[test]
    fn a_checkpoint_of_any_size_packs_into_frames_recovery_reads() {
        let (segments, fragments) = (300_000u32, 60_000u64);
        let records = (0..segments)
            .map(|segment| {
                LogRecord::Segment(SegmentRecord {
                    segment,
                    incarnation: 1,
                    state: SegmentState::Sealed,
                    write_pos: 4096,
                })
            })
            .chain((0..fragments).map(|n| {
                LogRecord::Put(PutRecord {
                    key: ChunkKey {
                        block: u128::from(n),
                        epoch: 1,
                        index: 0,
                    },
                    segment: 0,
                    incarnation: 1,
                    offset: 4096,
                    record_len: 4096,
                    chunk_offset: 0,
                    payload_len: 1,
                    payload_crc: 0,
                    sequence: n,
                    time_ns: 0,
                    flags: FLAG_FINAL,
                })
            }));
        let mut frames: Vec<Vec<LogRecord>> = Vec::new();
        pack(records, |frame| {
            frames.push(frame.to_vec());
            Ok::<(), ()>(())
        })
        .unwrap();
        let block = 4096usize;
        let mut space = 0u64;
        for frame in &frames {
            let encoded = frame::encode(KIND_CHECKPOINT_CHUNK, 1, 1, 7, frame, block).unwrap();
            assert!(encoded.len() as u64 <= MAX_FRAME_BYTES);
            space += encoded.len() as u64;
        }
        for pair in frames.windows(2) {
            let used: u64 = pair[0].iter().map(|r| r.encoded_len() as u64).sum();
            assert!(used + pair[1][0].encoded_len() as u64 > FRAME_PAYLOAD);
        }
        let flat: Vec<LogRecord> = frames.concat();
        assert_eq!(flat.len(), segments as usize + fragments as usize);
        assert!(
            matches!(flat[segments as usize - 1], LogRecord::Segment(s) if s.segment == segments - 1)
        );
        assert!(matches!(flat[segments as usize], LogRecord::Put(p) if p.sequence == 0));
        // The end frame is one block.
        space += block as u64;
        assert!(space <= checkpoint_bytes(fragments, u64::from(segments), block as u64).unwrap());
        let mut empty = 0;
        pack(std::iter::empty(), |frame| {
            assert!(frame.is_empty());
            empty += 1;
            Ok::<(), ()>(())
        })
        .unwrap();
        assert_eq!(empty, 1);
    }

    fn limits() -> Limits {
        Limits {
            batch_requests: 2,
            batch_bytes: 100,
            fragments_per_chunk: 4,
        }
    }

    #[test]
    fn the_queue_holds_two_batches_and_refuses_beyond() {
        let (limits, mut q) = (limits(), Queue::default());
        for _ in 0..4 {
            q.admit(10, &limits).unwrap();
        }
        assert!(matches!(q.admit(0, &limits), Err(ChunkError::Busy)));
        q.release(10);
        q.admit(0, &limits).unwrap();
        let mut q = Queue::default();
        q.admit(150, &limits).unwrap();
        assert!(matches!(q.admit(51, &limits), Err(ChunkError::Busy)));
        q.admit(50, &limits).unwrap();
    }

    /// A payload larger than the whole byte bound is admitted into a queue holding no other
    /// payload, so the bound is its size or two batches, whichever is larger.
    #[test]
    fn a_payload_larger_than_the_bound_waits_for_an_empty_queue() {
        let (limits, mut q) = (limits(), Queue::default());
        q.admit(500, &limits).unwrap();
        assert!(matches!(q.admit(1, &limits), Err(ChunkError::Busy)));
        q.release(500);
        q.admit(0, &limits).unwrap();
        q.admit(500, &limits).unwrap();
        assert!(matches!(q.admit(1, &limits), Err(ChunkError::Busy)));
    }

    /// A held request whose answer goes nowhere.
    fn held() -> Request {
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        Request {
            op: Op::Checkpoint,
            reply: Reply::new(tx, None),
            queued: None,
        }
    }

    /// Writes held for the cleaner's verdict run again, all of them, once a verdict has come
    /// since the first of them was found blocked, though a later write was held after it came:
    /// taken as the latest hold read it, that verdict was spent and the first write waited for
    /// another that never came.
    #[test]
    fn writes_held_for_a_verdict_run_again_once_it_comes() {
        let mut waiting = Waiting::default();
        waiting.hold(held(), (7, 1));
        assert!(waiting.due((7, 1)).is_none());
        assert_eq!(waiting.due((9, 1)).map(|run| run.len()), Some(1));
        // One held against the verdict 7; the cleaner gives 9; another is held against 9.
        waiting.hold(held(), (7, 1));
        waiting.hold(held(), (9, 1));
        assert_eq!(waiting.due((9, 1)).map(|run| run.len()), Some(2));
        // A segment freed runs them again as a verdict does; with none held, nothing is due.
        waiting.hold(held(), (9, 1));
        assert_eq!(waiting.due((9, 2)).map(|run| run.len()), Some(1));
        assert!(waiting.due((11, 3)).is_none());
    }
}
