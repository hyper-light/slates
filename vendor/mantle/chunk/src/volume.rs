//! A volume: format it, open it (recovering from whatever state it was left in), write and
//! delete chunks through its group-commit writer, and read them back verified.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, RwLock};
use std::task::Waker;
use std::thread::JoinHandle;

use hyper_block::DiskError;
use hyper_block::block::BlockFile;
use hyper_block::buf::{AlignedBuf, Pool};
use hyper_block::issuer::{Attached, Issuer};

use crate::clean::{CleanReport, Cleaner};
use crate::error::ChunkError;
use crate::frame::SegmentState;
use crate::index::{Fragment, Segments};
use crate::key::ChunkKey;
use crate::layout::{Config, Geometry, MAX_FRAME_BYTES};
use crate::read;
use crate::record::Payload;
use crate::recover::{self, ReadPool, RecoveryReport, read_span};
use crate::scrub::{Findings, Scrubber, SharedFindings, scrub_all};
use crate::superblock::{OFFSET_A, OFFSET_B_COMPACT, OFFSET_B_STANDARD, Superblock};
use crate::writer::{
    INCARNATION_RESERVE, Op, Reply, Request, SEQUENCE_RESERVE, Shared, Writer, write_superblocks,
};

/// A request's answer, once it is durable or refused.
#[derive(Debug)]
pub struct Answer {
    answer: Receiver<Result<(), ChunkError>>,
}

impl Answer {
    pub fn wait(&self) -> Result<(), ChunkError> {
        self.answer.recv().map_err(|_| ChunkError::Closed)?
    }

    /// The answer if it has come.
    pub fn poll(&self) -> Option<Result<(), ChunkError>> {
        match self.answer.try_recv() {
            Ok(answer) => Some(answer),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(ChunkError::Closed)),
        }
    }
}

/// A chunk's size and state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkStat {
    pub len: u64,
    pub sealed: bool,
    pub fragments: usize,
}

/// How the volume's segments are used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub segments: u32,
    pub free: u32,
    pub open: u32,
    pub sealed: u32,
    /// Bytes of records some chunk still references.
    pub live_bytes: u64,
    /// Checkpoints written since the volume opened.
    pub checkpoints: u64,
}

/// Room a request holds in the writer's queue; given back if the request is never sent,
/// and by the writer once it takes the request off the queue.
struct Ticket<'a> {
    queue: &'a std::sync::Mutex<crate::writer::Queue>,
    bytes: u64,
    sent: bool,
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if !self.sent
            && let Ok(mut queue) = self.queue.lock()
        {
            queue.release(self.bytes);
        }
    }
}

/// A volume's file is read by its callers and its writer, cleaner and scrubber threads at once,
/// so it is `Sync`, as a device file's descriptor is; its writes go through the device's issuer,
/// on a duplicate handle of the issuer's own.
/// One caller's buffers for the aligned reads [`Volume::read_into`] makes from the device,
/// kept between its reads. Each caller owns its own, and no reader shares buffers with
/// another: hyper-block's pool has one owner, the thread that issues the reads (hyper-block
/// buf.rs). It keeps at most a batch's bytes free, what the caller's own reads took.
pub struct ReadBuffers(ReadPool);

pub struct Volume<F: BlockFile + Sync + 'static> {
    shared: Arc<Shared<F>>,
    sender: Option<SyncSender<Request>>,
    writer: Option<JoinHandle<()>>,
    /// Wakes the cleaner, which stops once `stopping` is set.
    wake_cleaner: Option<SyncSender<()>>,
    cleaner: Option<JoinHandle<()>>,
    wake_scrubber: Option<SyncSender<()>>,
    scrubber: Option<JoinHandle<()>>,
    findings: SharedFindings,
    superblock: Superblock,
    limits: crate::layout::Limits,
}

impl<F: BlockFile + Sync + 'static> Volume<F> {
    /// Lays out a new, empty volume of `size` bytes in `file` and opens it, its writes going
    /// through `issuer`, the issuer of the device `file` is on. Whatever `file` held before is
    /// ignored: every structure is bound to the new volume's random id.
    pub fn format(issuer: &Issuer, file: F, size: u64, config: Config) -> Result<Self, ChunkError> {
        config.check()?;
        let geometry = Geometry::plan(size, file.alignment(), &config)?;
        let mut io = issuer.attach(&file).map_err(ChunkError::Device)?;
        if config.prewrite {
            prewrite(
                &mut io,
                file.alignment(),
                &geometry,
                config.limits.batch_bytes,
            )?;
        }
        let volume = random_id()?;
        let created_ns = now_ns();
        let base = Superblock {
            block: u32::try_from(geometry.block).map_err(|_| ChunkError::Config("block".into()))?,
            volume,
            sequence: 0,
            created_ns,
            segment_size: geometry.segment_size,
            segments: geometry.segments,
            checksum_shift: config.checksum_shift,
            offset_b: geometry.offset_b,
            log_offset: geometry.log_offset,
            log_size: geometry.log_size,
            data_offset: geometry.data_offset,
            start_lsn: 1,
            start_pos: 0,
            // No checkpoint yet: nothing the replay must reach.
            end_lsn: 1,
            // Nothing issued, nothing reserved: the first batch reserves.
            sequence_limit: 0,
            incarnation_limit: 0,
        };
        let a = base.clone();
        let b = Superblock {
            sequence: 1,
            ..base
        };
        write_superblocks(&mut io, file.alignment(), &[&a, &b])?;
        let (volume, _) = Self::start(file, io, b, geometry, config, usize::MAX)?;
        Ok(volume)
    }

    /// Opens a volume, recovering from a clean shutdown or a crash alike, its writes going
    /// through `issuer`, the issuer of the device `file` is on.
    pub fn open(
        issuer: &Issuer,
        file: F,
        config: Config,
    ) -> Result<(Self, RecoveryReport), ChunkError> {
        Self::open_starting(issuer, file, config, usize::MAX)
    }

    /// `open`, starting at most `may_start` of the volume's threads: the seam that tests a
    /// start the operating system refuses part way.
    fn open_starting(
        issuer: &Issuer,
        file: F,
        config: Config,
        may_start: usize,
    ) -> Result<(Self, RecoveryReport), ChunkError> {
        config.check()?;
        let superblock = read_superblock(&file)?;
        let geometry = Geometry::of(&superblock);
        if !file.alignment().is_aligned_u64(geometry.block) {
            return Err(ChunkError::Format(format!(
                "volume block {} is not a multiple of the file's alignment {}",
                geometry.block,
                file.alignment().get()
            )));
        }
        let config = Config {
            segment_size: geometry.segment_size,
            checksum_shift: superblock.checksum_shift,
            ..config
        };
        geometry.holds(&config)?;
        let io = issuer.attach(&file).map_err(ChunkError::Device)?;
        Self::start(file, io, superblock, geometry, config, may_start)
    }

    fn start(
        file: F,
        io: Attached,
        superblock: Superblock,
        geometry: Geometry,
        config: Config,
        may_start: usize,
    ) -> Result<(Self, RecoveryReport), ChunkError> {
        // Recovery's read buffers, the writer's once the volume runs.
        let batch = config.limits.batch_bytes;
        let mut reads = ReadPool::of_batch(file.alignment(), batch);
        let mut recovered = recover::recover(&file, &mut reads, &superblock, &geometry, &config)?;
        // Recovery continues one open segment per stream. One found open beyond those is
        // sealed where recovery left its end, and the checkpoint below records it, so no
        // batch's frame carries more than its requests do (layout::batch_frame_payload).
        let mut opens = std::mem::take(&mut recovered.open).into_iter();
        let (client, clean, whole) = (opens.next(), opens.next(), opens.next());
        let mut sealed = 0usize;
        for segment in opens {
            if let Some(info) = usize::try_from(segment)
                .ok()
                .and_then(|i| recovered.segments.get_mut(i))
            {
                info.state = SegmentState::Sealed;
                sealed = sealed.saturating_add(1);
            }
        }
        let segments = Segments::new(recovered.segments, geometry.block);
        let shared = Arc::new(Shared {
            file,
            geometry,
            volume: superblock.volume,
            checksum_shift: superblock.checksum_shift,
            index: RwLock::new(recovered.index),
            fenced: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            dead_bytes: std::sync::atomic::AtomicU64::new(0),
            futile_at: std::sync::atomic::AtomicU64::new(u64::MAX),
            submitted: std::sync::atomic::AtomicU64::new(0),
            clean_opened: std::sync::atomic::AtomicU64::new(0),
            clean_open: std::sync::atomic::AtomicU64::new(crate::writer::open_at(
                [client, clean, whole],
                crate::writer::Stream::Clean,
            )),
            checkpoints: std::sync::atomic::AtomicU64::new(0),
            queue: std::sync::Mutex::new(crate::writer::Queue::default()),
            peak_rate: std::sync::atomic::AtomicU64::new(0),
            cleaning: std::sync::Mutex::new(()),
            service_ns: std::sync::atomic::AtomicU64::new(0),
            clean_ns: std::sync::atomic::AtomicU64::new(0),
            low_water: std::sync::atomic::AtomicUsize::new(crate::clean::runway(
                0,
                0,
                0,
                u64::try_from(batch).unwrap_or(u64::MAX),
                geometry.segment_size,
            )),
            usage: RwLock::new(segments.clone()),
            reads: crate::read::Gate::new(config.reads),
        });
        // Client requests are admitted up to `queue_requests` (`submit`); the cleaner sends
        // one request at a time beside them.
        let (sender, rx) = sync_channel(config.limits.queue_requests().saturating_add(1));
        let (wake, wakes) = sync_channel(1);
        let report = recovered.report;
        let mut writer = Writer {
            shared: Arc::clone(&shared),
            config,
            rx,
            segments,
            opens: [client, clean, whole],
            incarnation: recovered.incarnation,
            sequence: recovered.sequence,
            cursor: recovered.cursor,
            superblock: superblock.clone(),
            fragments: recovered.fragments,
            poke: Some(wake.clone()),
            // Two batches' buffers kept free, each up to twice a batch.
            pool: Pool::new(
                shared.file.alignment(),
                batch.saturating_mul(2),
                batch.saturating_mul(2),
            ),
            reads,
            io,
            anticipation: hyper_block::commit::Anticipation::new(),
            received: 0,
            unconfirmed: Vec::new(),
            waiting: crate::writer::Waiting::default(),
        };
        // Recovery changed the index relative to the log: it put relocations back to the
        // copies they moved, indexed records the log never named, or sealed segments left
        // open. These decisions live only in memory until a checkpoint records them; without
        // one, a later replay would move the relocations again, forget the rolled-forward
        // records and find the segments open.
        if report.restored > 0 || report.rolled_forward > 0 || sealed > 0 {
            writer.checkpoint()?;
        }
        let superblock = writer.superblock.clone();
        let cleaner = Cleaner {
            shared: Arc::clone(&shared),
            submit: sender.clone(),
            wake: wakes,
            batch_bytes: config.limits.batch_bytes,
            batch_moves: config.limits.batch_requests,
        };
        let findings: SharedFindings = Arc::new(std::sync::Mutex::new(Findings::default()));
        let (wake_scrubber, scrubber) = match config.scrub_period {
            Some(period) => {
                let (wake, wakes) = sync_channel(1);
                let scrubber = Scrubber {
                    shared: Arc::clone(&shared),
                    findings: Arc::clone(&findings),
                    wake: wakes,
                    period,
                    reads: ReadPool::of_batch(shared.file.alignment(), batch),
                };
                (Some(wake), Some(scrubber))
            }
            None => (None, None),
        };
        // The volume owns each thread from the moment it starts. A thread the operating
        // system refuses returns the error through `?`, and dropping the volume then stops
        // and joins those already running (audit S12): left detached, the writer would hold
        // the cleaner's wake sender and the cleaner the writer's queue, each waiting on the
        // other for good with the device held open.
        let mut volume = Self {
            shared,
            sender: Some(sender),
            writer: None,
            wake_cleaner: Some(wake),
            cleaner: None,
            wake_scrubber,
            scrubber: None,
            findings,
            superblock,
            limits: config.limits,
        };
        let mut threads = Threads { left: may_start };
        volume.writer = Some(threads.start("mantle-chunk-writer", move || writer.run())?);
        volume.cleaner = Some(threads.start("mantle-chunk-cleaner", move || cleaner.run())?);
        if let Some(scrubber) = scrubber {
            volume.scrubber = Some(threads.start("mantle-chunk-scrubber", move || scrubber.run())?);
        }
        Ok((volume, report))
    }

    pub fn volume_id(&self) -> u128 {
        self.superblock.volume
    }

    /// The bytes of the file or device that hold segments.
    pub fn data_span(&self) -> std::ops::Range<u64> {
        let g = &self.shared.geometry;
        let end = u64::from(g.segments)
            .saturating_mul(g.segment_size)
            .saturating_add(g.data_offset);
        g.data_offset..end
    }

    pub fn is_fenced(&self) -> bool {
        self.shared.fenced.load(Ordering::Acquire)
    }

    /// Writes a whole chunk and seals it. Returns once it is durable.
    pub fn put(&self, key: ChunkKey, data: &[u8]) -> Result<(), ChunkError> {
        self.write(key, 0, data, true, None)
    }

    /// Writes a whole chunk like [`Volume::put`], after checking it against the CRC-32C its
    /// sender computed: bytes changed on the way are refused before anything is written.
    pub fn put_checked(&self, key: ChunkKey, data: &[u8], crc32c: u32) -> Result<(), ChunkError> {
        self.write(key, 0, data, true, Some(crc32c))
    }

    /// Appends to a chunk at `offset`, which must be where the chunk ends (or an exact
    /// repeat of a fragment already there). `seal` makes this the chunk's last fragment.
    pub fn append(
        &self,
        key: ChunkKey,
        offset: u64,
        data: &[u8],
        seal: bool,
    ) -> Result<(), ChunkError> {
        self.write(key, offset, data, seal, None)
    }

    /// Appends like [`Volume::append`], after checking the bytes against the CRC-32C their
    /// sender computed.
    pub fn append_checked(
        &self,
        key: ChunkKey,
        offset: u64,
        data: &[u8],
        seal: bool,
        crc32c: u32,
    ) -> Result<(), ChunkError> {
        self.write(key, offset, data, seal, Some(crc32c))
    }

    /// Checksums the payload in the calling thread, verifies it against `expected` when the
    /// sender supplied one, and submits it. A payload no record can hold, or one the queue has
    /// no room for, is refused before it is copied.
    fn write(
        &self,
        key: ChunkKey,
        offset: u64,
        data: &[u8],
        seal: bool,
        expected: Option<u32>,
    ) -> Result<(), ChunkError> {
        let (op, ticket) = self.write_op(key, offset, data, seal, expected)?;
        self.send(op, ticket)
    }

    /// The write `write` submits, admitted to the queue.
    fn write_op(
        &self,
        key: ChunkKey,
        offset: u64,
        data: &[u8],
        seal: bool,
        expected: Option<u32>,
    ) -> Result<(Op, Ticket<'_>), ChunkError> {
        let ticket = self.admit_payload(data.len())?;
        let op = self.write_payload(key, offset, data.to_vec(), seal, expected)?;
        Ok((op, ticket))
    }

    /// Room in the writer's queue for a payload of `len` bytes: refused before anything is
    /// copied when no record can hold it, or the queue has no room.
    fn admit_payload(&self, len: usize) -> Result<Ticket<'_>, ChunkError> {
        let len = u64::try_from(len).unwrap_or(u64::MAX);
        let max = crate::writer::max_payload(&self.shared.geometry, self.shared.checksum_shift);
        if len > max {
            return Err(ChunkError::TooLarge { len, max });
        }
        self.admit(len)
    }

    /// The write of `data` at `offset` of `key`'s chunk, its checksums computed in the calling
    /// thread and checked against `expected` when the sender supplied one.
    fn write_payload(
        &self,
        key: ChunkKey,
        offset: u64,
        data: Vec<u8>,
        seal: bool,
        expected: Option<u32>,
    ) -> Result<Op, ChunkError> {
        let payload = Payload::new(data, self.shared.checksum_shift)
            .ok_or_else(|| ChunkError::Config("checksum block size".into()))?;
        if let Some(expected) = expected
            && expected != payload.crc
        {
            return Err(ChunkError::Checksum {
                key,
                expected,
                actual: payload.crc,
            });
        }
        Ok(Op::Write {
            key,
            offset,
            payload,
            seal,
        })
    }

    /// Deletes a chunk; deleting one that does not exist succeeds.
    pub fn delete(&self, key: ChunkKey) -> Result<(), ChunkError> {
        self.submit(Op::Delete { key })
    }

    /// Writes a whole chunk as [`Volume::put`] does, but returns once the request is queued:
    /// the answer comes through the returned [`Answer`], and `waker` is woken exactly once,
    /// when it has come or when the volume closes without answering. One thread can so keep
    /// many requests out (docs/design/node.md §1.3, measurement.md §10).
    pub fn put_waking(
        &self,
        key: ChunkKey,
        data: &[u8],
        waker: Waker,
    ) -> Result<Answer, ChunkError> {
        let (op, ticket) = self.write_op(key, 0, data, true, None)?;
        self.dispatch(op, ticket, Some(waker))
    }

    /// Writes a whole chunk as [`Volume::put_checked`] does, from bytes the caller hands over
    /// rather than lends, and answers as [`Volume::put_waking`] does: a storage node writes a
    /// chunk it was sent from the buffer the chunk arrived in, checked against the CRC-32C its
    /// sender computed, without a copy and without its thread waiting for the flush (node.md
    /// §5.2). Refused before anything is checksummed when no record can hold it or the queue
    /// has no room.
    pub fn put_checked_waking(
        &self,
        key: ChunkKey,
        data: Vec<u8>,
        crc32c: u32,
        waker: Waker,
    ) -> Result<Answer, ChunkError> {
        let ticket = self.admit_payload(data.len())?;
        let op = self.write_payload(key, 0, data, true, Some(crc32c))?;
        self.dispatch(op, ticket, Some(waker))
    }

    /// Deletes a chunk as [`Volume::delete`] does, answering as [`Volume::put_waking`] does.
    pub fn delete_waking(&self, key: ChunkKey, waker: Waker) -> Result<Answer, ChunkError> {
        let ticket = self.admit(0)?;
        self.dispatch(Op::Delete { key }, ticket, Some(waker))
    }

    /// Writes the whole index into the log and points the superblock at it, so the next open
    /// replays nothing written before now. Returns once it is durable.
    pub fn checkpoint(&self) -> Result<(), ChunkError> {
        self.submit(Op::Checkpoint)
    }

    fn submit(&self, op: Op) -> Result<(), ChunkError> {
        let ticket = self.admit(0)?;
        self.send(op, ticket)
    }

    /// Takes room in the writer's queue for a request of `bytes` payload bytes: `Busy` when
    /// the queue holds all the writer takes (`Limits::queue_requests`, `queue_bytes`).
    fn admit(&self, bytes: u64) -> Result<Ticket<'_>, ChunkError> {
        if self.is_fenced() {
            return Err(ChunkError::Fenced);
        }
        self.shared
            .queue
            .lock()
            .map_err(|_| ChunkError::Fenced)?
            .admit(bytes, &self.limits)?;
        Ok(Ticket {
            queue: &self.shared.queue,
            bytes,
            sent: false,
        })
    }

    /// Sends an admitted request and waits for its answer.
    fn send(&self, op: Op, ticket: Ticket<'_>) -> Result<(), ChunkError> {
        self.dispatch(op, ticket, None)?.wait()
    }

    /// Sends an admitted request; its answer comes through what is returned.
    fn dispatch(
        &self,
        op: Op,
        mut ticket: Ticket<'_>,
        waker: Option<Waker>,
    ) -> Result<Answer, ChunkError> {
        let sender = self.sender.as_ref().ok_or(ChunkError::Closed)?;
        let (reply, answer) = sync_channel(1);
        self.shared.submitted.fetch_add(1, Ordering::AcqRel);
        let request = Request {
            op,
            reply: Reply::new(reply, waker),
            queued: Some(ticket.bytes),
        };
        match sender.try_send(request) {
            Ok(()) => ticket.sent = true,
            // Admission keeps the channel from filling; if it has, refuse rather than wait.
            Err(TrySendError::Full(_)) => return Err(ChunkError::Busy),
            Err(TrySendError::Disconnected(_)) => return Err(ChunkError::Closed),
        }
        Ok(Answer { answer })
    }

    pub fn stat(&self, key: &ChunkKey) -> Result<Option<ChunkStat>, ChunkError> {
        let index = self.shared.index.read().map_err(|_| ChunkError::Fenced)?;
        Ok(index.get(key).map(|e| ChunkStat {
            len: e.len(),
            sealed: e.sealed,
            fragments: e.fragments.len(),
        }))
    }

    /// Every chunk key, sorted.
    pub fn keys(&self) -> Result<Vec<ChunkKey>, ChunkError> {
        let index = self.shared.index.read().map_err(|_| ChunkError::Fenced)?;
        let mut keys: Vec<ChunkKey> = index.iter().map(|(k, _)| *k).collect();
        keys.sort_unstable();
        Ok(keys)
    }

    /// Bytes of the index log.
    pub fn log_bytes(&self) -> u64 {
        self.shared.geometry.log_size
    }

    /// The most payload bytes one record holds: a segment less its header and the record's.
    /// Requests the writer's queue holds (`Limits::queue_requests`): past them a write is
    /// refused `Busy`.
    pub fn queue_requests(&self) -> usize {
        self.limits.queue_requests()
    }

    /// Payload bytes the writer's queue holds (`Limits::queue_bytes`): past them a write is
    /// refused `Busy`, so a storage node holds no more of the chunks it is sent than its volumes
    /// can queue.
    pub fn queue_bytes(&self) -> u64 {
        self.limits.queue_bytes()
    }

    /// The bytes the volume's segments hold, which chunks are written into: what placement
    /// weighs it by (docs/research/63).
    pub fn data_bytes(&self) -> u64 {
        let geometry = &self.shared.geometry;
        geometry
            .segment_size
            .saturating_mul(u64::from(geometry.segments))
    }

    pub fn max_payload(&self) -> u64 {
        crate::writer::max_payload(&self.shared.geometry, self.shared.checksum_shift)
    }

    pub fn usage(&self) -> Result<Usage, ChunkError> {
        let segments = self.shared.usage.read().map_err(|_| ChunkError::Fenced)?;
        Ok(Usage {
            segments: u32::try_from(segments.count()).unwrap_or(u32::MAX),
            free: u32::try_from(segments.free_count()).unwrap_or(u32::MAX),
            open: segments.open_count(),
            sealed: segments.sealed_count(),
            live_bytes: segments.live(),
            checkpoints: self.shared.checkpoints.load(Ordering::Relaxed),
        })
    }

    /// Reads `len` bytes of a chunk from `offset`, verifying every byte returned against the
    /// record's checksums and every record against the identity the index expects. The read
    /// takes a turn at the device, waiting behind at most `Reads::waiting` others, and is
    /// refused with `Busy` past them (docs/design/chunk-store.md §7).
    pub fn read(&self, key: &ChunkKey, offset: u64, len: u64) -> Result<Vec<u8>, ChunkError> {
        let mut out = Vec::new();
        self.read_into(key, offset, len, &mut out, &mut self.read_buffers())?;
        Ok(out)
    }

    /// Buffers for one caller's reads ([`ReadBuffers`]).
    pub fn read_buffers(&self) -> ReadBuffers {
        ReadBuffers(ReadPool::of_batch(
            self.shared.file.alignment(),
            self.limits.batch_bytes,
        ))
    }

    /// Reads like [`Volume::read`] into `out`, replacing what it held, through the caller's
    /// own `buffers`. A caller that reads in a loop with the same `out` and `buffers` takes no
    /// new buffer from the allocator for its bytes once they are large enough.
    pub fn read_into(
        &self,
        key: &ChunkKey,
        offset: u64,
        len: u64,
        out: &mut Vec<u8>,
        buffers: &mut ReadBuffers,
    ) -> Result<(), ChunkError> {
        let found = self.fragments(key, offset, len)?;
        self.read_from(key, (offset, len), &found, out, buffers)
    }

    /// Reads bytes `[offset, offset + len)` of `key`'s chunk from the fragments the index named
    /// when the read began. The cleaner may have moved a record since and the writer freed its
    /// segment and written it again, so a record that fails its check where it was found is
    /// looked for again: one moved or deleted since is no damage, as the scrubber judges
    /// (`scrub::damaged_owner`), and the read is made again where the chunk lies now. Moved
    /// again while that read waited, the chunk is answered `Busy`, a cause that may pass; still
    /// where it was found, the failure is the record's.
    pub(crate) fn read_from(
        &self,
        key: &ChunkKey,
        (offset, len): (u64, u64),
        found: &[Fragment],
        out: &mut Vec<u8>,
        buffers: &mut ReadBuffers,
    ) -> Result<(), ChunkError> {
        let failed = match self.read_fragments(key, (offset, len), found, out, buffers) {
            Err(e @ ChunkError::Corrupt { .. }) => e,
            read => return read,
        };
        let now = self.fragments(key, offset, len)?;
        if now == found {
            return Err(failed);
        }
        match self.read_fragments(key, (offset, len), &now, out, buffers) {
            Err(ChunkError::Corrupt { .. }) if self.fragments(key, offset, len)? != now => {
                Err(ChunkError::Busy)
            }
            read => read,
        }
    }

    /// Reads bytes `[offset, offset + len)` of `key`'s chunk from `fragments` into `out`,
    /// verified, taking a turn at the device for them.
    fn read_fragments(
        &self,
        key: &ChunkKey,
        (offset, len): (u64, u64),
        fragments: &[Fragment],
        out: &mut Vec<u8>,
        buffers: &mut ReadBuffers,
    ) -> Result<(), ChunkError> {
        out.clear();
        let end = offset.saturating_add(len);
        // The fragments are read one at a time, so the read buffers the most any one needs.
        let mut cost = 0u64;
        for fragment in fragments {
            let from = offset
                .max(fragment.chunk_offset)
                .saturating_sub(fragment.chunk_offset);
            let to = end
                .min(fragment.end())
                .saturating_sub(fragment.chunk_offset);
            let plan = read::Plan::new(
                self.shared.checksum_shift,
                fragment,
                from,
                to,
                self.shared.reads.gap(),
            )
            .ok_or(ChunkError::Corrupt {
                key: *key,
                detail: "record size".into(),
            })?;
            cost = cost.max(plan.bytes());
        }
        // Let through before anything is taken: a read refused or waiting holds no memory.
        let _turn = self.shared.reads.enter(cost)?;
        let capacity =
            usize::try_from(len).map_err(|_| ChunkError::TooLarge { len, max: u64::MAX })?;
        out.try_reserve(capacity)
            .map_err(|_| ChunkError::TooLarge { len, max: 0 })?;
        for fragment in fragments {
            let from = offset
                .max(fragment.chunk_offset)
                .saturating_sub(fragment.chunk_offset);
            let to = end
                .min(fragment.end())
                .saturating_sub(fragment.chunk_offset);
            read::fragment(&self.shared, &mut buffers.0, key, fragment, from, to, out)?;
        }
        Ok(())
    }

    /// The fragments that hold bytes `[offset, offset + len)` of `key`'s chunk, in order:
    /// `NotFound` for a chunk the index does not hold, `Range` for bytes past its end.
    pub(crate) fn fragments(
        &self,
        key: &ChunkKey,
        offset: u64,
        len: u64,
    ) -> Result<Vec<Fragment>, ChunkError> {
        let index = self.shared.index.read().map_err(|_| ChunkError::Fenced)?;
        let entry = index.get(key).ok_or(ChunkError::NotFound(*key))?;
        let end = offset.checked_add(len);
        if end.is_none_or(|e| e > entry.len()) {
            return Err(ChunkError::Range {
                key: *key,
                offset,
                len,
                chunk_len: entry.len(),
            });
        }
        let end = offset.saturating_add(len);
        Ok(entry.covering(offset, end).copied().collect())
    }

    /// What readers share with the writer: the file, its geometry and the index.
    pub(crate) fn shared(&self) -> &Shared<F> {
        &self.shared
    }

    /// A reader of this volume's chunks for an owner on a runtime task, through `issuer`, the
    /// issuer of the device the volume is on: its reads go to the device as the issuer's
    /// batches and are taken as their answers come, so its owner's thread never waits on the
    /// device (docs/design/node.md §1.3). It keeps as many reads out as the volume's measured
    /// read depth (`Reads::depth`), and refuses one past them with `Busy`.
    pub fn reader(&self, issuer: &Issuer) -> Result<crate::Reader, ChunkError> {
        let reads = self.shared.reads.reads();
        let attached = issuer
            .attach_deep(&self.shared.file, reads.depth)
            .map_err(ChunkError::Device)?;
        Ok(crate::Reader::new(
            attached,
            self.shared.file.alignment(),
            self.limits.batch_bytes,
            reads,
        ))
    }

    /// What the read gate has let through and refused since the volume opened.
    pub fn read_stats(&self) -> Result<crate::ReadStats, ChunkError> {
        self.shared.reads.stats()
    }

    /// Verifies every stored fragment now; returns the number that failed. Failed chunks are
    /// listed by `damaged`.
    pub fn scrub(&self) -> Result<u64, ChunkError> {
        let mut reads = ReadPool::of_batch(self.shared.file.alignment(), self.limits.batch_bytes);
        scrub_all(&self.shared, &mut reads, &self.findings)
    }

    /// Chunks with a fragment that failed verification, found by reads of the scrubber, for
    /// repair from another copy.
    pub fn damaged(&self) -> Result<Vec<ChunkKey>, ChunkError> {
        let findings = self.findings.lock().map_err(|_| ChunkError::Fenced)?;
        Ok(findings.damaged.iter().copied().collect())
    }

    /// Whether damage has been found since the volume opened: errors cluster, so a volume
    /// with one is scrubbed continuously and should be repaired or drained.
    pub fn at_risk(&self) -> bool {
        self.findings
            .lock()
            .map(|f| f.at_risk_since.is_some())
            .unwrap_or(true)
    }

    /// Whether the volume has more damage than repair chunk by chunk should chase, and is to
    /// be drained whole (`scrub::MAX_DAMAGED`).
    pub fn failing(&self) -> bool {
        self.findings.lock().map(|f| f.failing).unwrap_or(true)
    }

    /// Forgets a chunk once it has been repaired or its copy here deleted.
    pub fn repaired(&self, key: &ChunkKey) -> Result<(), ChunkError> {
        self.findings
            .lock()
            .map_err(|_| ChunkError::Fenced)?
            .damaged
            .remove(key);
        Ok(())
    }

    /// Cleans the `n` segments with the best cost-benefit ratio now, whatever the free
    /// space, and waits for their space to be freed.
    pub fn clean(&self, n: u32) -> Result<CleanReport, ChunkError> {
        let submit = self.sender.clone().ok_or(ChunkError::Closed)?;
        let (_, wakes) = sync_channel(1);
        let cleaner = Cleaner {
            shared: Arc::clone(&self.shared),
            submit,
            wake: wakes,
            batch_bytes: self.limits.batch_bytes,
            batch_moves: self.limits.batch_requests,
        };
        let mut reads = cleaner.reads();
        cleaner.clean_best(&mut reads, n)
    }

    /// Stops the writer after it answers what is queued. Everything acknowledged is already
    /// durable, so closing writes nothing.
    pub fn close(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        // The cleaner holds a sender to the writer, so it stops first. The writer holds a
        // wake sender of its own, so the cleaner is told to stop and then woken.
        self.shared.stopping.store(true, Ordering::Release);
        if let Some(wake) = self.wake_cleaner.take() {
            let _ = wake.try_send(());
        }
        if let Some(handle) = self.cleaner.take() {
            // A thread that unwound has already stopped; there is nothing more to wait for.
            let _ = handle.join();
        }
        if let Some(wake) = self.wake_scrubber.take() {
            let _ = wake.try_send(());
        }
        if let Some(handle) = self.scrubber.take() {
            let _ = handle.join();
        }
        self.sender.take();
        if let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
    }
}

impl<F: BlockFile + Sync + 'static> Drop for Volume<F> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Starts a volume's threads, as many as the operating system makes and at most `left`
/// more: the bound is the seam that tests a refused start.
struct Threads {
    left: usize,
}

impl Threads {
    fn start(
        &mut self,
        name: &str,
        run: impl FnOnce() + Send + 'static,
    ) -> Result<JoinHandle<()>, ChunkError> {
        let started = match self.left.checked_sub(1) {
            Some(left) => {
                self.left = left;
                std::thread::Builder::new().name(name.into()).spawn(run)
            }
            None => Err(std::io::Error::from(std::io::ErrorKind::OutOfMemory)),
        };
        started.map_err(|source| {
            ChunkError::Device(DiskError::Io {
                op: "start a thread",
                path: std::path::PathBuf::new(),
                source,
            })
        })
    }
}

/// Reads both superblock copies (and both possible places for B) and returns the valid one
/// with the highest sequence.
/// Writes zeros over the whole volume before its superblocks, so its first appends overwrite
/// written blocks where the file system journals an extent's first write
/// (docs/design/chunk-store.md §2). Each transfer is the writer's largest batch, the most the
/// store writes at once; the flush that makes the superblocks durable makes these durable too.
fn prewrite(
    io: &mut Attached,
    align: hyper_block::buf::Alignment,
    geometry: &Geometry,
    batch_bytes: usize,
) -> Result<(), ChunkError> {
    let end = geometry
        .end()
        .ok_or_else(|| ChunkError::Config("volume too large".into()))?;
    let step = align.down(batch_bytes).max(align.get());
    let zeroed = |len: usize| {
        let mut zeros = AlignedBuf::zeroed(len, align).map_err(|e| ChunkError::Device(e.into()))?;
        zeros
            .set_len(len)
            .map_err(|e| ChunkError::Device(e.into()))?;
        Ok::<_, ChunkError>(zeros)
    };
    let mut zeros = zeroed(step)?;
    let step = u64::try_from(step).map_err(|_| ChunkError::Config("batch too large".into()))?;
    // Every region starts and ends on a block, so each transfer is aligned; the loop runs
    // `end / step` times, rounded up, each transfer going out and its buffer coming back.
    let mut at = 0u64;
    while at < end {
        let len = step.min(end.saturating_sub(at));
        let buf = if len == step {
            zeros
        } else {
            zeroed(usize::try_from(len).map_err(|_| ChunkError::Config("batch too large".into()))?)?
        };
        zeros = io
            .write(vec![(buf, at)], false)
            .map_err(ChunkError::Device)?
            .pop()
            .map(|(b, _)| b)
            .ok_or(ChunkError::Internal(
                "a pre-write's buffer did not come back",
            ))?;
        at = at.saturating_add(len);
    }
    Ok(())
}

/// What the volume in `file` fixed at format, as its newest superblock records it: where
/// everything is, and its records' checksum blocks, which it keeps when opened again with any
/// configuration (`Volume::open`).
pub fn formatted<F: BlockFile>(file: &F) -> Result<(Geometry, u8), ChunkError> {
    let superblock = read_superblock(file)?;
    Ok((Geometry::of(&superblock), superblock.checksum_shift))
}

/// The valid superblock copy with the higher sequence. When its twin does not read, it may be
/// the older copy, one write behind a newer one now damaged or torn, whose reservations it
/// lacks (writer.rs `reserve`): records may carry numbers past its own. The newer copy's are
/// at most one batch's numbers and one reservation past them, and a batch issues fewer
/// sequences than one frame holds bytes and opens at most every segment. So the reservations
/// are taken past that, and no number that might be on the device is issued twice. The next
/// superblock written goes to the twin's slot and rewrites it.
fn read_superblock<F: BlockFile>(file: &F) -> Result<Superblock, ChunkError> {
    let mut best: Option<Superblock> = None;
    // Offsets of the copies that read, whichever block size they were read at.
    let mut read = Vec::new();
    let len = file.len().map_err(ChunkError::Device)?;
    let mut pool = ReadPool::new(Pool::new(file.alignment(), 64 << 10, 64 << 10));
    for offset in [OFFSET_A, OFFSET_B_COMPACT, OFFSET_B_STANDARD] {
        for block in [4096u64, 8192, 16384, 32768, 65536] {
            if offset.saturating_add(block) > len || !file.alignment().is_aligned_u64(block) {
                continue;
            }
            let base = Geometry {
                block,
                segment_size: 0,
                segments: 0,
                offset_b: 0,
                log_offset: 0,
                log_size: 0,
                data_offset: 0,
            };
            let Ok(Some(span)) = read_span(file, &mut pool, &base, 0, offset, block) else {
                continue;
            };
            let decoded = Superblock::decode(span.bytes());
            pool.give(span);
            if let Some(sb) = decoded
                && sb.offset_of(sb.slot()) == offset
            {
                read.push((sb.volume, offset));
                if best.as_ref().is_none_or(|b| sb.sequence > b.sequence) {
                    best = Some(sb);
                }
            }
        }
    }
    let mut sb = best.ok_or_else(|| ChunkError::Format("no valid superblock".into()))?;
    let twin = sb.offset_of(1u8.saturating_sub(sb.slot()));
    if !read.contains(&(sb.volume, twin)) {
        let past = || ChunkError::Format("superblock reservations past u64".into());
        sb.sequence_limit = sb
            .sequence_limit
            .checked_add(SEQUENCE_RESERVE)
            .and_then(|l| l.checked_add(MAX_FRAME_BYTES))
            .ok_or_else(past)?;
        sb.incarnation_limit = sb
            .incarnation_limit
            .checked_add(INCARNATION_RESERVE)
            .and_then(|l| l.checked_add(u64::from(sb.segments)))
            .ok_or_else(past)?;
    }
    Ok(sb)
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A random 128-bit volume id from the OS's generator.
fn random_id() -> Result<u128, ChunkError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| ChunkError::Config(format!("random id: {e}")))?;
    Ok(u128::from_le_bytes(bytes))
}

impl<F: BlockFile + Sync + 'static> Volume<F> {
    /// Each segment's state, incarnation, write position and live bytes.
    pub fn segments(&self) -> Result<Vec<(SegmentState, u64, u32, u64)>, ChunkError> {
        let usage = self.shared.usage.read().map_err(|_| ChunkError::Fenced)?;
        Ok(usage
            .iter()
            .map(|(_, s)| (s.state, s.incarnation, s.write_pos, s.live))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_block::buf::Alignment;

    use crate::device::{Handle, SimDevice, sim};
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc::Receiver;
    use std::sync::{Condvar, Mutex};

    use crate::frame::LogRecord;
    use crate::layout::{Limits, Reads};

    /// The issuer of the one device every volume of these tests is on, keeping four
    /// transfers in flight, as a device measured there.
    fn issuer() -> &'static Issuer {
        static ISSUER: std::sync::OnceLock<Issuer> = std::sync::OnceLock::new();
        ISSUER.get_or_init(|| Issuer::start(std::path::Path::new("test device"), 4).unwrap())
    }

    /// A simulated file whose flushes wait while its gate is shut, so a test can queue
    /// requests behind a batch the writer is flushing, and know when it is. State: whether
    /// the gate is shut, flushes begun, and flushes the shut gate still lets through.
    struct Gated {
        file: SimDevice,
        state: Mutex<(bool, u64, u64)>,
        changed: Condvar,
    }

    impl Gated {
        fn new(file: SimDevice) -> Handle<Self> {
            Handle::new(Self {
                file,
                state: Mutex::new((false, 0, 0)),
                changed: Condvar::new(),
            })
        }

        fn shut(&self, shut: bool) {
            self.state.lock().unwrap().0 = shut;
            self.changed.notify_all();
        }

        /// Waits until a flush has begun since `seen` flushes, and returns the new count.
        fn flushing_after(&self, seen: u64) -> u64 {
            let mut state = self.state.lock().unwrap();
            while state.1 <= seen {
                state = self.changed.wait(state).unwrap();
            }
            state.1
        }

        fn flushes(&self) -> u64 {
            self.state.lock().unwrap().1
        }

        /// Opens the gate when dropped, so a test that fails with it shut ends rather than
        /// leaving the writer waiting while the volume's drop joins it.
        fn reopen_on_drop(gated: &Handle<Self>) -> impl Drop {
            struct Reopen(Handle<Gated>);
            impl Drop for Reopen {
                fn drop(&mut self) {
                    if let Ok(mut state) = self.0.state.lock() {
                        state.0 = false;
                    }
                    self.0.changed.notify_all();
                }
            }
            Reopen(gated.clone())
        }

        /// Lets `n` more flushes through the shut gate.
        fn pass(&self, n: u64) {
            self.state.lock().unwrap().2 += n;
            self.changed.notify_all();
        }
    }

    impl BlockFile for Gated {
        fn alignment(&self) -> Alignment {
            self.file.alignment()
        }
        fn len(&self) -> Result<u64, DiskError> {
            self.file.len()
        }
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<(), DiskError> {
            self.file.read_exact_at(buf, offset)
        }
        fn write_all_at(&self, buf: &[u8], offset: u64) -> Result<(), DiskError> {
            self.file.write_all_at(buf, offset)
        }
        fn sync_data(&self) -> Result<(), DiskError> {
            let mut state = self.state.lock().unwrap();
            state.1 += 1;
            self.changed.notify_all();
            while state.0 && state.2 == 0 {
                state = self.changed.wait(state).unwrap();
            }
            if state.0 {
                state.2 -= 1;
            }
            drop(state);
            self.file.sync_data()
        }
    }

    /// Queues a put without waiting for its answer, as `write` does before it waits.
    fn queue_put<F: BlockFile + Sync + 'static>(
        v: &Volume<F>,
        key: ChunkKey,
        data: &[u8],
    ) -> Receiver<Result<(), ChunkError>> {
        let len = data.len() as u64;
        let mut ticket = v.admit(len).unwrap();
        let payload = Payload::new(data.to_vec(), v.shared.checksum_shift).unwrap();
        let (reply, answer) = sync_channel(1);
        v.sender
            .as_ref()
            .unwrap()
            .try_send(Request {
                op: Op::Write {
                    key,
                    offset: 0,
                    payload,
                    seal: true,
                },
                reply: Reply::new(reply, None),
                queued: Some(len),
            })
            .map_err(|_| ())
            .unwrap();
        ticket.sent = true;
        answer
    }

    fn key(n: u64) -> ChunkKey {
        ChunkKey {
            block: u128::from(n),
            epoch: 1,
            index: 0,
        }
    }

    /// Queues a delete without waiting for its answer.
    fn queue_delete<F: BlockFile + Sync + 'static>(
        v: &Volume<F>,
        key: ChunkKey,
    ) -> Receiver<Result<(), ChunkError>> {
        let mut ticket = v.admit(0).unwrap();
        let (reply, answer) = sync_channel(1);
        v.sender
            .as_ref()
            .unwrap()
            .try_send(Request {
                op: Op::Delete { key },
                reply: Reply::new(reply, None),
                queued: Some(0),
            })
            .map_err(|_| ())
            .unwrap();
        ticket.sent = true;
        answer
    }

    /// Requests that write nothing, retries of a put already durable, come in while a delete
    /// waits for a later frame to confirm its batch's. They write no frame, and before, the
    /// delete was confirmed only when the queue next ran empty, so a steady stream of them
    /// held it, and every answer behind it, for as long as it lasted (audit S15). Now a batch
    /// that writes no frame confirms at once: the confirmation's flush begins while the later
    /// batches are still queued.
    #[test]
    fn a_batch_that_writes_nothing_confirms_the_delete_before_it() {
        let file = Gated::new(sim(902));
        let batch = 8;
        let volume =
            Volume::format(issuer(), file.clone(), 128 << 20, batch_config(batch)).unwrap();
        volume.put(key(1), b"retried").unwrap();
        volume.put(key(2), b"deleted").unwrap();
        let _reopen = Gated::reopen_on_drop(&file);
        file.shut(true);
        let seen = file.flushes();
        let delete = queue_delete(&volume, key(2));
        file.flushing_after(seen);
        // All the queue holds besides the delete: more than one batch.
        let retries: Vec<_> = (1..volume.limits.queue_requests())
            .map(|_| queue_put(&volume, key(1), b"retried"))
            .collect();
        file.pass(1);
        file.flushing_after(seen + 1);
        let queued = volume.shared.queue.lock().unwrap().requests();
        file.shut(false);
        delete.recv().unwrap().unwrap();
        for retry in retries {
            retry.recv().unwrap().unwrap();
        }
        assert!(
            queued > 0,
            "the delete was confirmed only once the queue ran empty"
        );
    }

    /// What a cleaning pass gains is the victims it frees less the segments its relocations
    /// open. It was measured as the change in free segments, which client writes during the
    /// pass spend as well, so a pass that freed a segment while a client opened one looked
    /// futile, and a futile pass answers client writes `Full` while space could still be
    /// reclaimed. Here two half-dead victims pack into one segment, and a client write that
    /// opens a segment lands between the relocation and the victims' freeing.
    #[test]
    fn a_pass_gains_what_it_frees_whatever_clients_write_meanwhile() {
        let file = Gated::new(sim(907));
        let config = Config {
            segment_size: 256 << 10,
            checksum_shift: 12,
            max_fragments: 2000,
            compact: true,
            scrub_period: None,
            limits: Limits {
                batch_requests: 64,
                batch_bytes: 1 << 20,
                fragments_per_chunk: 64,
            },
            prewrite: false,
            reads: Reads::default(),
        };
        let volume = Arc::new(Volume::format(issuer(), file.clone(), 8 << 20, config).unwrap());
        let chunk = vec![3u8; 50 << 10];
        // Four chunks a segment: two sealed segments and a third open.
        for n in 0..9 {
            volume.put(key(n), &chunk).unwrap();
        }
        for n in [0, 1, 4, 5] {
            volume.delete(key(n)).unwrap();
        }
        let _reopen = Gated::reopen_on_drop(&file);
        file.shut(true);
        let seen = file.flushes();
        let cleaning = {
            let volume = Arc::clone(&volume);
            std::thread::spawn(move || volume.clean(2))
        };
        file.flushing_after(seen);
        // Too large for the client's open segment: it seals it and opens another.
        let client = queue_put(&volume, key(100), &vec![5u8; 250 << 10]);
        file.shut(false);
        client.recv().unwrap().unwrap();
        let report = cleaning.join().unwrap().unwrap();
        assert_eq!(report.segments, 2, "{report:?}");
        assert_eq!(report.gained, 1, "{report:?}");
    }

    /// A record that died after a clean listed its place is no damage, whatever lies there now.
    /// A batch frees a segment whose records all died and may open it again in the same frame,
    /// writing there before the segment table says so (`Writer::layout`, `Writer::publish`): the
    /// cleaner read those bytes while the table still named the old incarnation and counted them
    /// corrupt (CI's 2-CPU busy job, `corrupt: 2`). It judges as the scrubber does.
    #[test]
    fn a_place_whose_record_died_is_no_damage_while_its_segment_is_written_again() {
        let file = Gated::new(sim(912));
        let config = Config {
            segment_size: 256 << 10,
            checksum_shift: 12,
            max_fragments: 2000,
            compact: true,
            scrub_period: None,
            limits: Limits {
                batch_requests: 64,
                batch_bytes: 1 << 20,
                fragments_per_chunk: 64,
            },
            prewrite: false,
            reads: Reads::default(),
        };
        let volume = Volume::format(issuer(), file.clone(), 8 << 20, config).unwrap();
        let chunk = vec![3u8; 50 << 10];
        // Four chunks fill the first segment; the fifth opens the next.
        for n in 0..5 {
            volume.put(key(n), &chunk).unwrap();
        }
        let first = volume
            .shared
            .index
            .read()
            .unwrap()
            .get(&key(0))
            .unwrap()
            .fragments[0];
        let (segment, incarnation) = (first.segment, first.incarnation);
        let cleaner = Cleaner {
            shared: Arc::clone(&volume.shared),
            submit: volume.sender.clone().unwrap(),
            wake: sync_channel(1).1,
            batch_bytes: volume.limits.batch_bytes,
            batch_moves: volume.limits.batch_requests,
        };
        let places = cleaner.places(segment).unwrap();
        assert_eq!(places.len(), 4);
        for n in 0..3 {
            volume.delete(key(n)).unwrap();
        }
        // The last delete's batch held at its flush, a put too large for the open segment queued
        // behind it: once the delete is out, the next batch frees the segment and opens it again
        // for the put in one frame, its bytes written, held at its own flush.
        let _reopen = Gated::reopen_on_drop(&file);
        file.shut(true);
        let seen = file.flushes();
        let delete = queue_delete(&volume, key(3));
        file.flushing_after(seen);
        let put = queue_put(&volume, key(100), &vec![5u8; 220 << 10]);
        file.pass(1);
        file.flushing_after(seen + 1);
        let named = volume
            .shared
            .usage
            .read()
            .unwrap()
            .get(segment)
            .map(|s| (s.state, s.incarnation));
        let mut reads = cleaner.reads();
        let identity = recover::Identity {
            volume: volume.shared.volume,
            checksum_shift: volume.shared.checksum_shift,
        };
        let there = recover::verify_at(
            &volume.shared.file,
            &mut reads,
            &volume.shared.geometry,
            identity,
            segment,
            u64::from(places[0]),
            None,
        )
        .unwrap()
        .map(|v| v.prefix.header.incarnation);
        let cleaned = cleaner.clean(&mut reads, segment, incarnation, places);
        file.shut(false);
        delete.recv().unwrap().unwrap();
        put.recv().unwrap().unwrap();
        assert_eq!(named, Some((SegmentState::Sealed, incarnation)));
        assert!(
            there.is_some_and(|i| i > incarnation),
            "the place holds the put's record, of a later incarnation: {there:?}"
        );
        assert_eq!(cleaned.unwrap(), (0, 0));
    }

    /// With the newer superblock copy unreadable, the older one opens the volume, and its
    /// reservations may be a write behind: records may carry sequences and incarnations past
    /// them, and resuming at them would issue those numbers again. The reservations read are
    /// at least the newer copy's.
    #[test]
    fn reservations_read_from_the_older_copy_cover_the_newer_ones() {
        let file = sim(908);
        let volume = Volume::format(issuer(), file.clone(), 128 << 20, batch_config(8)).unwrap();
        volume.put(key(1), b"reserves").unwrap();
        volume.close();
        let newer = read_superblock(&file).unwrap();
        assert!(newer.sequence_limit > 0 && newer.incarnation_limit > 0);
        file.inject(hyper_block::sim::Fault::BitFlip {
            offset: newer.offset_of(newer.slot()) + 40,
            bit: 1,
            stored: true,
        })
        .unwrap();
        let older = read_superblock(&file).unwrap();
        assert!(older.sequence < newer.sequence, "the older copy was read");
        assert!(older.sequence_limit >= newer.sequence_limit, "{older:?}");
        assert!(
            older.incarnation_limit >= newer.incarnation_limit,
            "{older:?}"
        );
    }

    /// A second delete of a chunk finds it gone: removed by a first delete not yet confirmed,
    /// in an earlier batch or earlier in its own. Its answer rests on the first's, and before,
    /// it was answered at once; with the confirmation, or the batch, then failing, the first
    /// was refused and the second said the chunk was deleted, which after a crash and damage
    /// to the first's frame it was not. It now waits for the first and fails with it.
    #[test]
    fn a_repeated_delete_is_answered_with_the_first() {
        for same_batch in [false, true] {
            let sim = sim(903);
            let file = Gated::new(sim.clone());
            let volume =
                Volume::format(issuer(), file.clone(), 128 << 20, batch_config(8)).unwrap();
            volume.put(key(5), b"chunk").unwrap();
            let _reopen = Gated::reopen_on_drop(&file);
            file.shut(true);
            let seen = file.flushes();
            let (first, second) = if same_batch {
                let _held = queue_put(&volume, key(6), b"ahead");
                file.flushing_after(seen);
                (queue_delete(&volume, key(5)), queue_delete(&volume, key(5)))
            } else {
                let first = queue_delete(&volume, key(5));
                file.flushing_after(seen);
                (first, queue_delete(&volume, key(5)))
            };
            // The flush being held completes; every write after it fails.
            sim.inject(hyper_block::sim::Fault::PowerCut { ops: 1 })
                .unwrap();
            file.shut(false);
            assert!(first.recv().unwrap().is_err(), "same batch {same_batch}");
            let second = second.recv().unwrap();
            assert!(
                second.is_err(),
                "same batch {same_batch}: the repeat was answered {second:?} and the first failed"
            );
        }
    }

    fn batch_config(requests: usize) -> Config {
        Config {
            segment_size: 16 << 20,
            checksum_shift: 12,
            max_fragments: requests as u64 + 1,
            compact: true,
            scrub_period: None,
            limits: Limits {
                batch_requests: requests,
                batch_bytes: 1 << 20,
                fragments_per_chunk: 1,
            },
            prewrite: false,
            reads: Reads::default(),
        }
    }

    /// Puts `requests` one-byte chunks as one batch, behind a first put that fills its
    /// segment, so the batch opens the next segment and only its frame says so: roll-forward,
    /// which rescans the segments the log left open, cannot find records in a segment whose
    /// opening the log lost. Every put must be acknowledged, and every one must survive a
    /// reopen.
    fn one_batch_survives_reopening(file: &Handle<Gated>, config: Config, requests: usize) {
        let volume = Volume::format(issuer(), file.clone(), 128 << 20, config).unwrap();
        file.shut(true);
        let seen = file.flushes();
        let whole = crate::writer::max_payload(&volume.shared.geometry, config.checksum_shift);
        let first = queue_put(&volume, key(0), &vec![7u8; whole as usize]);
        file.flushing_after(seen);
        let answers: Vec<_> = (1..=requests as u64)
            .map(|n| queue_put(&volume, key(n), b"y"))
            .collect();
        file.shut(false);
        first.recv().unwrap().unwrap();
        for answer in answers {
            answer.recv().unwrap().unwrap();
        }
        volume.close();
        let (volume, report) = Volume::open(issuer(), file.clone(), config).unwrap();
        let kept = (0..=requests as u64)
            .filter(|&n| volume.stat(&key(n)).unwrap().is_some())
            .count();
        assert_eq!(
            kept,
            requests + 1,
            "acknowledged puts lost on reopen; {} frames replayed",
            report.frames
        );
    }

    /// The audit's reproduction of S13: 70,000 one-byte puts in one batch make a 5.3 MB
    /// frame, which recovery, reading no frame past 4 MiB, took for the log's end: every
    /// put was acknowledged and all but the first were gone after a reopen. The settings
    /// are now refused before any I/O.
    #[test]
    fn a_batch_whose_frame_recovery_cannot_read_is_refused_before_any_io() {
        let file = sim(9);
        let refused = Volume::format(issuer(), file.clone(), 128 << 20, batch_config(70_000));
        assert!(matches!(refused, Err(ChunkError::Config(_))));
        assert_eq!(file.stats().unwrap().writes, 0);
    }

    /// Were the accounting that keeps batches within the frame bound wrong, the batch whose
    /// frame recovery could not read fails, typed, and is never written: nothing it held is
    /// acknowledged, and what was is kept (audit S13). The volume is started past the
    /// settings check, as nothing but a fault in it could.
    #[test]
    fn a_frame_past_the_bound_fails_its_batch_and_is_never_written() {
        let (legal, requests) = (17_000usize, 70_000usize);
        let sim = sim(15);
        let file = Gated::new(sim.clone());
        Volume::format(issuer(), file.clone(), 128 << 20, batch_config(legal))
            .unwrap()
            .close();
        let superblock = read_superblock(&file).unwrap();
        let geometry = Geometry::of(&superblock);
        let unchecked = batch_config(requests);
        assert!(unchecked.check().is_err());
        let (volume, _) = Volume::start(
            file.clone(),
            issuer().attach(&file).unwrap(),
            superblock,
            geometry,
            unchecked,
            usize::MAX,
        )
        .unwrap();
        file.shut(true);
        let seen = file.flushes();
        let whole = crate::writer::max_payload(&geometry, unchecked.checksum_shift);
        let first = queue_put(&volume, key(0), &vec![7u8; whole as usize]);
        file.flushing_after(seen);
        let answers: Vec<_> = (1..=requests as u64)
            .map(|n| queue_put(&volume, key(n), b"y"))
            .collect();
        file.shut(false);
        first.recv().unwrap().unwrap();
        let failed = answers
            .into_iter()
            .map(|a| a.recv().unwrap())
            .filter(|r| matches!(r, Err(ChunkError::Internal(_) | ChunkError::Fenced)))
            .count();
        assert_eq!(failed, requests);
        assert!(volume.is_fenced());
        volume.close();
        let (volume, _) = Volume::open(issuer(), file.clone(), batch_config(legal)).unwrap();
        assert_eq!(volume.read(&key(0), 0, 1).unwrap(), [7u8]);
        assert!(volume.stat(&key(1)).unwrap().is_none());
    }

    /// The largest batch the settings allow is acknowledged and reopens whole.
    #[test]
    fn the_largest_batch_the_settings_allow_reopens_whole() {
        let largest = (1..=70_000usize)
            .rev()
            .find(|&n| batch_config(n).check().is_ok())
            .unwrap();
        assert!(batch_config(largest + 1).check().is_err());
        let file = Gated::new(sim(10));
        one_batch_survives_reopening(&file, batch_config(largest), largest);
    }

    /// Every batch frame the writer makes stays within the bound its settings are checked
    /// against (audit S13), under eight writers whose puts each seal a segment and open the
    /// next, deletes that leave segments to free, and the cleaner's passes beside them until
    /// they finish. No two of the eight's records share a segment, so moving them gains
    /// nothing; the cleaner's relocations come from small chunks put first, three in four
    /// deleted. The log's frames are read back from every block where one can start.
    #[test]
    fn batch_frames_stay_within_the_bound_their_settings_are_checked_against() {
        let requests = 8usize;
        let config = Config {
            segment_size: 16 << 10,
            checksum_shift: 12,
            max_fragments: 4096,
            compact: true,
            scrub_period: None,
            limits: Limits {
                batch_requests: requests,
                batch_bytes: 1 << 20,
                fragments_per_chunk: 64,
            },
            prewrite: false,
            reads: Reads::default(),
        };
        let file = sim(11);
        let volume = Volume::format(issuer(), file.clone(), 48 << 20, config).unwrap();
        // Two such records never share a segment.
        let len = crate::writer::max_payload(&volume.shared.geometry, 12) as usize / 2 + 1;
        for i in 0..150u64 {
            let n = 100_000 + i;
            volume.put(key(n), &vec![n as u8; len / 8]).unwrap();
            if i % 4 != 0 {
                volume.delete(key(n)).unwrap();
            }
        }
        let mut cleaned = std::thread::scope(|s| {
            let writers: Vec<_> = (0..8u64)
                .map(|t| {
                    let v = &volume;
                    s.spawn(move || {
                        for i in 0..150u64 {
                            let n = t * 1000 + i;
                            loop {
                                match v.put(key(n), &vec![n as u8; len]) {
                                    Err(ChunkError::Busy) => std::thread::yield_now(),
                                    other => break other.unwrap(),
                                }
                            }
                            if i >= 2 {
                                v.delete(key(n - 2)).unwrap();
                            }
                        }
                    })
                })
                .collect();
            let mut cleaned = CleanReport::default();
            loop {
                let report = volume.clean(8).unwrap();
                cleaned.relocated += report.relocated;
                cleaned.corrupt += report.corrupt;
                if writers.iter().all(|w| w.is_finished()) {
                    break cleaned;
                }
                std::thread::yield_now();
            }
        });
        let last = volume.clean(64).unwrap();
        cleaned.relocated += last.relocated;
        cleaned.corrupt += last.corrupt;
        // Victims whose records all died while being cleaned, freed and opened again by
        // another stream, are not corrupt.
        assert!(cleaned.relocated > 0 && cleaned.corrupt == 0, "{cleaned:?}");
        let geometry = volume.shared.geometry;
        let id = volume.volume_id();
        volume.close();
        let bound = crate::layout::batch_frame_payload(requests, requests).unwrap();
        let one = crate::layout::batch_frame_payload(1, 0).unwrap();
        let (mut batches, mut largest) = (0u64, 0u64);
        let mut log = crate::log::Frames::new(&file, &geometry, id).unwrap();
        let mut pos = 0u64;
        while pos < geometry.log_size {
            if let Some((header, records, _)) = log.frame(pos).unwrap()
                && header.kind == crate::frame::KIND_BATCH
            {
                let payload: u64 = records.iter().map(|r| r.encoded_len() as u64).sum();
                assert!(payload <= bound, "{payload} bytes of records past {bound}");
                largest = largest.max(payload);
                batches += 1;
            }
            pos += geometry.block;
        }
        assert!(batches > 0);
        // Batches of several requests formed, so the bound was tested past one request's.
        assert!(largest > one, "{largest} bytes at most");
    }

    /// A checkpoint whose records fill frames to their bound reopens whole: its first frame
    /// carries the segments' states and the first fragments, the next the rest (audit S13).
    #[test]
    fn a_checkpoint_of_full_frames_reopens_whole() {
        let fragments = 60_000u64;
        let config = Config {
            segment_size: 1 << 20,
            checksum_shift: 12,
            max_fragments: fragments + 1,
            compact: true,
            scrub_period: None,
            limits: Limits::default(),
            prewrite: false,
            reads: Reads::default(),
        };
        let file = sim(12);
        let volume = Volume::format(issuer(), file.clone(), 96 << 20, config).unwrap();
        let wave = config.limits.queue_requests() as u64;
        let mut n = 0u64;
        while n < fragments {
            let end = (n + wave).min(fragments);
            let answers: Vec<_> = (n..end).map(|k| queue_put(&volume, key(k), b"z")).collect();
            for answer in answers {
                answer.recv().unwrap().unwrap();
            }
            n = end;
        }
        volume.checkpoint().unwrap();
        volume.close();
        let (volume, report) = Volume::open(issuer(), file.clone(), config).unwrap();
        // The beginning, a chunk and the end.
        assert!(report.frames >= 3, "{report:?}");
        for k in 0..fragments {
            assert_eq!(volume.read(&key(k), 0, 1).unwrap(), b"z");
        }
    }

    /// Settings the log, sized at format, cannot hold are refused when the volume is
    /// opened, before anything is written (audit S13).
    #[test]
    fn settings_the_log_was_not_sized_for_are_refused_at_open() {
        let file = sim(13);
        let volume = Volume::format(issuer(), file.clone(), 8 << 20, config()).unwrap();
        volume.put(key(1), b"kept").unwrap();
        volume.close();
        let writes = file.stats().unwrap().writes;
        let mut larger = config();
        larger.limits.batch_requests = 16_000;
        larger.check().unwrap();
        assert!(matches!(
            Volume::open(issuer(), file.clone(), larger),
            Err(ChunkError::Config(_))
        ));
        assert_eq!(file.stats().unwrap().writes, writes);
        let (volume, _) = Volume::open(issuer(), file.clone(), config()).unwrap();
        assert_eq!(volume.read(&key(1), 0, 4).unwrap(), b"kept");
    }

    /// A new write whose record is longer than half a segment goes to a stream of its own, so the
    /// smaller ones keep the segment they began: two records of 60 KiB, one of 200 KiB that does
    /// not fit after them in 256 KiB, and a third of 60 KiB fill two segments, both still open,
    /// where the 200 KiB record sealed the first with 136 KiB of it empty and the third sealed
    /// the second with 52 KiB.
    #[test]
    fn a_record_longer_than_half_a_segment_takes_a_segment_of_its_own() {
        let mut settings = config();
        settings.scrub_period = None;
        let volume = Volume::format(issuer(), sim(14), 8 << 20, settings).unwrap();
        let sizes = [60usize << 10, 60 << 10, 200 << 10, 60 << 10];
        for (n, size) in (1u64..).zip(sizes) {
            volume.put(key(n), &vec![n as u8; size]).unwrap();
        }
        let used: Vec<SegmentState> = volume
            .segments()
            .unwrap()
            .into_iter()
            .map(|(state, ..)| state)
            .filter(|state| *state != SegmentState::Free)
            .collect();
        assert_eq!(used, [SegmentState::Open, SegmentState::Open], "{used:?}");
        for (n, size) in (1u64..).zip(sizes) {
            let len = size as u64;
            assert_eq!(volume.read(&key(n), 0, len).unwrap(), vec![n as u8; size]);
        }
    }

    /// A volume filled with records longer than half a segment holds one a segment, each with
    /// room beside it that no other such record fits. The cleaner's estimate (`Cleaner::gain`)
    /// counts that room as space packing gives back, and a pass near full moved record after
    /// record, each into a segment of its own, before it found it had gained nothing, while
    /// puts waited on it: ten seconds a volume of 8 MiB chunks (2026-10-28). A pass lays out
    /// its relocations first now, and moves nothing.
    #[test]
    fn a_pass_that_could_only_move_records_around_moves_none() {
        let mut settings = config();
        settings.scrub_period = None;
        let volume = Volume::format(issuer(), sim(15), 8 << 20, settings).unwrap();
        // 200 KiB in 256 KiB segments: one a segment, with 52 KiB beside it.
        let len = 200usize << 10;
        let mut written = 0u64;
        loop {
            match volume.put(key(written), &vec![written as u8; len]) {
                Ok(()) => written += 1,
                Err(ChunkError::Full) => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert!(written > 8, "{written} records");
        let report = volume.clean(64).unwrap();
        assert_eq!((report.segments, report.relocated), (0, 0), "{report:?}");
        // Nor did the background cleaner, woken as free segments ran low.
        assert_eq!(volume.shared.clean_opened.load(Ordering::Relaxed), 0);
        for n in 0..written {
            let want = vec![n as u8; len];
            assert_eq!(volume.read(&key(n), 0, len as u64).unwrap(), want);
        }
    }

    /// Every pass that moves a record gains a segment: its plan lays out the relocations as the
    /// writer does. Chunks from a few hundred bytes to most of a segment, a third of them
    /// deleted again, fill a volume and leave segments partly dead, cleaned now and then.
    #[test]
    fn every_pass_that_moves_records_gains_a_segment() {
        let mut settings = config();
        settings.scrub_period = None;
        let volume = Volume::format(issuer(), sim(16), 8 << 20, settings).unwrap();
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut live: Vec<(u64, usize)> = Vec::new();
        let (mut cleaned, mut gained) = (0u32, 0u32);
        for n in 0..2000u64 {
            let len = match next() % 4 {
                0 => 200 + (next() % 4000) as usize,
                1 | 2 => (next() % (100 << 10)) as usize,
                _ => (130 << 10) + (next() % (100 << 10)) as usize,
            };
            match volume.put(key(n), &vec![n as u8; len]) {
                Ok(()) => live.push((n, len)),
                Err(ChunkError::Full) => {}
                Err(e) => panic!("{e}"),
            }
            if next() % 3 == 0 && !live.is_empty() {
                let (dead, _) = live.swap_remove((next() % live.len() as u64) as usize);
                volume.delete(key(dead)).unwrap();
            }
            if n % 50 == 49 {
                let report = volume.clean(8).unwrap();
                assert!(report.segments == 0 || report.gained >= 1, "{report:?}");
                cleaned += report.segments;
                gained += report.gained;
            }
        }
        assert!(gained > 0, "{cleaned} cleaned, {gained} gained");
        for (n, len) in live {
            assert_eq!(
                volume.read(&key(n), 0, len as u64).unwrap(),
                vec![n as u8; len]
            );
        }
    }

    /// A read that found a chunk's record before the cleaner moved it, its segment then freed and
    /// written again, reads the chunk where it lies now: the record that fails its check where it
    /// was found moved, and is no damage. It was answered `Corrupt` ("record identity does not
    /// match the index"), which the node answers a peer's get with.
    #[test]
    fn a_read_whose_record_the_cleaner_moved_reads_it_where_it_lies_now() {
        let mut settings = config();
        settings.scrub_period = None;
        let volume = Volume::format(issuer(), sim(17), 8 << 20, settings).unwrap();
        // Four chunks a segment: the first two segments, half dead, pack into one.
        let chunk = |n: u64| vec![n as u8; 50 << 10];
        for n in 0..12 {
            volume.put(key(n), &chunk(n)).unwrap();
        }
        for n in [0, 1, 4, 5] {
            volume.delete(key(n)).unwrap();
        }
        let len = 50u64 << 10;
        let found = volume.fragments(&key(2), 0, len).unwrap();
        let report = volume.clean(2).unwrap();
        assert_eq!((report.segments, report.gained), (2, 1), "{report:?}");
        assert_ne!(volume.fragments(&key(2), 0, len).unwrap(), found);
        // New chunks take the segments the cleaner freed.
        for n in 100..120 {
            volume.put(key(n), &chunk(n)).unwrap();
        }
        let mut buffers = volume.read_buffers();
        let mut out = Vec::new();
        let mut stale = Vec::new();
        assert!(matches!(
            volume.read_fragments(&key(2), (0, len), &found, &mut stale, &mut buffers),
            Err(ChunkError::Corrupt { .. })
        ));
        volume
            .read_from(&key(2), (0, len), &found, &mut out, &mut buffers)
            .unwrap();
        assert_eq!(out, chunk(2));
    }

    /// Segments recovery finds open beyond the writer's three streams, which no writer of this
    /// format leaves, are sealed when the volume starts and recorded by a checkpoint, so no
    /// batch carries them (audit S13). The log here is given a frame opening three more.
    #[test]
    fn open_segments_beyond_the_streams_are_sealed_at_start() {
        let mut settings = config();
        settings.scrub_period = None;
        let file = sim(14);
        let volume = Volume::format(issuer(), file.clone(), 8 << 20, settings).unwrap();
        volume.put(key(1), b"kept").unwrap();
        let geometry = volume.shared.geometry;
        volume.close();
        let superblock = read_superblock(&file).unwrap();
        let mut pool = ReadPool::new(Pool::new(file.alignment(), 1 << 20, 1 << 20));
        let recovered =
            recover::recover(&file, &mut pool, &superblock, &geometry, &settings).unwrap();
        let last = geometry.segments - 1;
        let opened: Vec<LogRecord> = [last, last - 1, last - 2]
            .into_iter()
            .zip(1u64..)
            .map(|(segment, i)| {
                LogRecord::Segment(crate::frame::SegmentRecord {
                    segment,
                    incarnation: recovered.incarnation + i,
                    state: SegmentState::Open,
                    write_pos: geometry.block as u32,
                })
            })
            .collect();
        let lsn = recovered.cursor.lsn;
        let frame = crate::frame::encode(
            crate::frame::KIND_BATCH,
            lsn,
            lsn,
            superblock.volume,
            &opened,
            geometry.block_usize(),
        )
        .unwrap();
        let mut frame_buf = AlignedBuf::zeroed(frame.len(), file.alignment()).unwrap();
        frame_buf.extend_from_slice(&frame).unwrap();
        let frame = frame_buf;
        file.write_all_at(frame.as_slice(), geometry.log_offset + recovered.cursor.pos)
            .unwrap();
        file.sync_data().unwrap();
        let open = |v: &Volume<SimDevice>| {
            v.segments()
                .unwrap()
                .iter()
                .filter(|(state, ..)| *state == SegmentState::Open)
                .count()
        };
        let (volume, _) = Volume::open(issuer(), file.clone(), settings).unwrap();
        assert_eq!(open(&volume), 3);
        volume.close();
        let (volume, _) = Volume::open(issuer(), file.clone(), settings).unwrap();
        assert_eq!(open(&volume), 3);
        assert_eq!(volume.read(&key(1), 0, 4).unwrap(), b"kept");
    }

    fn config() -> Config {
        Config {
            segment_size: 256 << 10,
            checksum_shift: 12,
            max_fragments: 2000,
            compact: true,
            scrub_period: Some(std::time::Duration::from_secs(3600)),
            limits: Limits {
                batch_requests: 64,
                batch_bytes: 1 << 20,
                fragments_per_chunk: 64,
            },
            prewrite: false,
            reads: Reads::default(),
        }
    }

    /// A simulated file that counts the reads that start in its log.
    struct Tally {
        file: SimDevice,
        log: Mutex<std::ops::Range<u64>>,
        log_reads: AtomicU64,
    }

    impl BlockFile for Tally {
        fn alignment(&self) -> Alignment {
            self.file.alignment()
        }
        fn len(&self) -> Result<u64, DiskError> {
            self.file.len()
        }
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<(), DiskError> {
            if self.log.lock().unwrap().contains(&offset) {
                self.log_reads.fetch_add(1, Ordering::SeqCst);
            }
            self.file.read_exact_at(buf, offset)
        }
        fn write_all_at(&self, buf: &[u8], offset: u64) -> Result<(), DiskError> {
            self.file.write_all_at(buf, offset)
        }
        fn sync_data(&self) -> Result<(), DiskError> {
            self.file.sync_data()
        }
    }

    /// Opening a volume reads its log a window at a time. It read each frame's first block
    /// and then the frame, and then every block of the log one at a time to prove no later
    /// frame was lost (audit P07); now the replay and the search each read the log in at most
    /// twice as many reads as it has windows, and the volume comes back whole.
    #[test]
    fn opening_reads_the_log_a_window_at_a_time() {
        let tally = Handle::new(Tally {
            file: sim(31),
            log: Mutex::new(0..0),
            log_reads: AtomicU64::new(0),
        });
        let settings = Config {
            max_fragments: 20_000,
            ..config()
        };
        let volume = Volume::format(issuer(), tally.clone(), 64 << 20, settings).unwrap();
        let chunks = 600u64;
        for n in 0..chunks {
            volume.put(key(n), &n.to_le_bytes()).unwrap();
        }
        let geometry = volume.shared.geometry;
        volume.close();
        *tally.log.lock().unwrap() = geometry.log_offset..geometry.log_offset + geometry.log_size;
        let (volume, report) = Volume::open(issuer(), tally.clone(), settings).unwrap();
        assert!(report.frames >= chunks, "{} frames replayed", report.frames);
        let windows = geometry.log_size.div_ceil(crate::layout::MAX_FRAME_BYTES);
        let reads = tally.log_reads.load(Ordering::SeqCst);
        assert!(
            reads <= 4 * windows,
            "{reads} reads of a log of {windows} windows holding {} frames",
            report.frames
        );
        for n in 0..chunks {
            assert_eq!(volume.read(&key(n), 0, 8).unwrap(), n.to_le_bytes());
        }
    }

    /// A start the operating system refuses after the writer, or the writer and the
    /// cleaner, have started stops and joins them before the error returns: nothing still
    /// holds the device, and the volume opens again as if the refusal had not happened
    /// (audit S12).
    #[test]
    fn a_start_refused_part_way_leaves_nothing_running() {
        // Every handle the volume takes is a clone of the test's, so the count of them is
        // the holds on the device.
        let file = Handle::new(sim(7));
        let key = ChunkKey {
            block: 1,
            epoch: 1,
            index: 0,
        };
        let volume = Volume::format(issuer(), file.clone(), 8 << 20, config()).unwrap();
        volume.put(key, b"acknowledged").unwrap();
        volume.close();
        // The writer, the cleaner and the scrubber start in that order; refuse each.
        for may_start in 0..3 {
            let refused = Volume::open_starting(issuer(), file.clone(), config(), may_start);
            assert!(matches!(
                refused,
                Err(ChunkError::Device(DiskError::Io { ref source, .. }))
                    if source.kind() == std::io::ErrorKind::OutOfMemory
            ));
            // Every thread that started has been joined, and with it every hold on the device.
            assert_eq!(Arc::strong_count(&file.0), 1, "refused at {may_start}");
        }
        let (volume, _) = Volume::open(issuer(), file.clone(), config()).unwrap();
        assert_eq!(volume.read(&key, 0, 12).unwrap(), b"acknowledged");
        volume.close();
        assert_eq!(Arc::strong_count(&file.0), 1);
    }
}
