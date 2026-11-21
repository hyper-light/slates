//! Maintenance workers: a shard's packing of full memtables and its trunk's compactions run on
//! threads of their own, so the shard's thread keeps taking puts while they run, as RocksDB's
//! flushes and compactions run on background threads beside its writers
//! (`max_background_jobs`; SILK, Balmau et al., USENIX ATC 2019, schedules them so). A job is a
//! compaction's inputs and range, or a full memtable's entries to pack; a worker runs it on its
//! own store over its own handle on the shard's file ([`Store::worker`]), writing into extents the
//! shard granted it, and hands back the branches made, named by the ticket the job's send
//! returned ([`Ticket`]): a worker freed while its result waits for its owner can take that
//! owner's next job, so the worker alone does not say which job a result answers. Nothing is
//! shared between threads: the shard keeps its memtables and trunk, a job owns its inputs'
//! descriptors, and a packing owns the copy of its memtable's arena, which the worker sorts
//! itself and gives back with the result for reuse ([`SortPack`]; research/40 §4). The trunk and the memtables change only on the shard's thread, when a
//! result is taken, so a read between never sees one half changed.
//!
//! How many workers: Little's law, L = λW (Little, Operations Research 9(3), 1961). Between one
//! period's start and the next (the trunk starts one when its root takes pending branches or a
//! consolidation starts), the shard's thread offers maintenance its memtables at the pace
//! puts fill them, and the workers spent `busy` nanoseconds on jobs; the shard itself ran for
//! `period` nanoseconds of that, its waits on the workers left out, since those measure too few
//! workers rather than demand. Keeping up takes `busy / period` workers on average, so the pool
//! admits the ceiling of that measured ratio as active jobs, at least one and at most the cores the OS reports
//! less the shard's own (`std::thread::available_parallelism`). A worker is started only when a
//! job is ready and none is free, so a pool never holds more threads than jobs were ever out at
//! once. Idle seats keep their warm stores and reservations until terminal retirement, so
//! reducing demand cannot overlap an old store's destruction with a replacement in its seat.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Instant;

use hyper_block::issuer::Attacher;
use slates_rt::sync::{ChannelReceiver, Sender};

use crate::branch::filter::Keys;
use crate::branch::merge::Compaction;
use crate::branch::{Branch, Builder};
use crate::error::Error;
use crate::memtable::hashed::Sorter;
use crate::store::cache::Kept;
use crate::store::{Ask, Config, IntoFile, Refill, Store, TopUp};
use hyper_block::block::BlockFile;

/// A compaction for a worker: `inputs` newest first, the descriptors a merge reads (no filters),
/// merged over `[from, end)` into branches of at most `per` entries, tombstones dropped when
/// `drop_tombstones`. `pages`: the pages the merge writes from its inputs at most, their
/// [`Branch::pages_between`] summed when it is planned, what its grant and the cache's room for
/// its writes are sized by.
#[derive(Debug)]
pub struct Task {
    pub inputs: Vec<Branch>,
    pub from: Vec<u8>,
    pub end: Option<Vec<u8>>,
    pub drop_tombstones: bool,
    pub per: u64,
    pub pages: usize,
}

/// A worker's work: a compaction, a full memtable to sort and pack into one branch, or a growth
/// of the shard's file.
#[derive(Debug)]
pub enum Work {
    Compact(Task),
    Sort(SortPack),
    Grow(Growth),
    Index(Box<IndexJob>),
}

/// The index over the memtables a shard keeps for gets (`crate::kept`, research/51 §7), out on a
/// worker to take the key lists of the memtables kept since it last covered one, oldest first,
/// after dropping, with `sweep`, the entries of memtables older than `oldest`, given back. It
/// comes back whole with its lists, which the other index takes next.
#[derive(Debug, Default)]
pub struct IndexJob {
    pub index: crate::kept::KeptIndex,
    pub lists: Vec<(u32, crate::kept::KeyList)>,
    pub oldest: u32,
    pub sweep: bool,
}

/// A full memtable packed by a worker that sorts it (research/40 §4): the copy of its arena it
/// kept as it was written ([`crate::memtable::hashed::HashMem::take_mirror`]), the first bytes
/// every key shares, the entries it holds (each key once), and the sorter's buffers. It comes
/// back with the result, its buffers kept for the next.
#[derive(Debug, Default)]
pub struct SortPack {
    pub entries: u64,
    pub skip: usize,
    pub arena: Vec<u8>,
    pub sorter: Sorter,
    /// Each kept key's filter hash and arena offset, written as the branch is built, for the
    /// index over the memtables the shard keeps for gets (`crate::kept`, research/51 §7).
    pub keys: crate::kept::KeyList,
    /// The root's pivot keys when the packing was handed out (`Trunk::root_cuts`): it ends a
    /// branch at each, so that each branch is one root pivot's (research/57 §7). None writes one
    /// branch.
    pub cuts: Vec<Vec<u8>>,
    /// Where each cut after the first falls among the sorted entries.
    pub ends: Vec<usize>,
}

impl SortPack {
    /// The bytes its buffers hold, at their capacities.
    pub fn memory(&self) -> usize {
        self.arena
            .capacity()
            .saturating_add(self.sorter.memory())
            .saturating_add(self.keys.bytes())
            .saturating_add(self.ends.capacity().saturating_mul(size_of::<usize>()))
    }
}

/// A segment of the shard's file to add, when `add`, and write whole, so its extents may be
/// handed out (`Store::grow_segment`, `Store::admit`): its first extent and its extents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Growth {
    pub start: u64,
    pub extents: u64,
    pub add: bool,
}

/// A job: its work, written into `grant`, with reads up to `file_end`, its pages sealed for the
/// checkpoint after `generation`; and the shard's cache's memory lent for the pages it writes
/// (`Kept`), which comes back with its result whatever the result.
#[derive(Debug)]
pub struct Job {
    pub work: Work,
    pub grant: Vec<u64>,
    pub file_end: u64,
    pub generation: u64,
    pub keep: Option<Box<Kept>>,
}

/// A job's result: the branches made, each with its first key, in key order; the granted
/// extents it never wrote; the file's end past its writes; the nanoseconds it ran; for a
/// compaction, the work units it merged in them, counted as a step on the shard counts its own
/// (keys merged, leaves opened and filter pages at their worth in keys), so the trunk prices a
/// compacted key from every compaction wherever it ran ([`super::Trunk::note_merge`]); and the
/// nanoseconds of them its reads waited for their pages, what its inputs not in the kernel's page
/// cache cost it ([`crate::store::IoStats::read_wait_ns`]).
#[derive(Debug)]
pub struct Output {
    pub parts: Vec<(Vec<u8>, Branch)>,
    pub unused: Vec<u64>,
    pub end: u64,
    pub ns: u64,
    pub units: u64,
    pub read_wait_ns: u64,
    /// The pages kept in the cache's loan, held with the result until its branches are applied:
    /// set by the owner from the job's [`Back`].
    pub kept: Option<Box<Kept>>,
}

/// Who a job's result goes back to: the trunk's maintenance, the shard's packing, or the shard's
/// file's growth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Trunk,
    Pack,
    Grow,
    Index,
}

/// A job handed to a worker: the worker, and the job's number among every job the pool has
/// handed out. A worker freed while its result waits
/// for its owner can take that owner's next job, so only the number tells two of an owner's jobs
/// apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub worker: usize,
    pub job: u64,
}

/// A job back, as the ticket its send returned names it: its result; the extents granted it
/// beyond its job's grant (its top-ups), which are the owner's to release if it failed; and a
/// sorted packing's buffers.
#[derive(Debug)]
pub struct Back {
    pub ticket: Ticket,
    pub result: Result<Output, Error>,
    /// The cache's memory the job was lent, its written pages in it: the owner's to adopt with
    /// the result, or to give back unread.
    pub kept: Option<Box<Kept>>,
    /// An issued-write failure or worker unwind remains visible when input was abandoned.
    pub physical_error: Option<Error>,
    pub topped: Vec<u64>,
    /// A sorted packing's buffers, back for the next.
    pub sorted: Option<SortPack>,
    /// An index job's index and lists, back.
    pub indexed: Option<Box<IndexJob>>,
}

/// What a worker tells the shard: its grant is spent and it waits for `n` more extents, or its
/// job is done.
#[derive(Debug)]
pub(crate) enum Message {
    /// Cold preparation acknowledges actual Store creation and issuer attachment.
    Ready {
        worker: usize,
        result: Result<(), Error>,
    },
    Need {
        worker: usize,
        n: usize,
    },
    Done {
        worker: usize,
        result: Result<Output, Error>,
        physical_error: Option<Error>,
        kept: Option<Box<Kept>>,
        sorted: Option<Box<SortPack>>,
        indexed: Option<Box<IndexJob>>,
    },
    /// Terminal native attachment result, after the owner's last Done was consumed.
    Retired {
        worker: usize,
        result: Result<(), Error>,
    },
    /// The outcome of the growth handed to a worker whose ask waited for it ([`TopUp::Grow`]);
    /// the worker then waits for its ask's answer.
    Grown {
        worker: usize,
        result: Result<(), Error>,
    },
}

/// The most messages one worker sends before the shard must take one, counted from [`serve`]:
/// an ask for extents waits for its answer before anything else is sent ([`Message::Need`]);
/// readiness goes before the first job ([`Message::Ready`]); a result is followed by a wait for
/// the next job, which the pool hands only once it has taken that result ([`Message::Done`]);
/// and a worker that unwound sends its result and at once its retirement's report
/// ([`Message::Retired`]), before the shard takes either. Two. The channel back holds this many
/// for every worker, so no send can wait on a shard that waits on that worker.
const MESSAGES_A_WORKER: usize = 2;

/// A worker's ends of its channels: its jobs, its grants' top-ups, and the shard's one channel
/// back, which holds [`MESSAGES_A_WORKER`] for each worker.
#[derive(Debug)]
pub struct Seat {
    pub id: usize,
    jobs: Receiver<Box<Job>>,
    more: Receiver<Result<TopUp, Error>>,
    back: Sender<Message>,
    /// The device's issuer and the batches the worker may have out on it, when the shard has
    /// one: its store's writes then go to the device while the worker builds on.
    attach: Option<(Attacher, usize)>,
    /// Only cold preparation reports startup before admitting a job.
    prepare: bool,
}

/// Starts a worker's thread on its seat: the shard's way to open its own file again.
pub type Spawn = Box<dyn FnMut(Seat) -> Result<JoinHandle<()>, Error> + Send>;

/// A worker's loop: each job run on one store, kept across jobs so its buffers stay warm, until
/// the shard drops its end of the job channel.
pub fn serve<F: BlockFile + 'static>(file: F, config: Config, seat: Seat) {
    let Seat {
        id,
        jobs,
        more,
        back,
        attach,
        prepare,
    } = seat;
    let worker = Store::worker(file, config).and_then(|mut store| {
        if let Some((attacher, batches)) = &attach {
            store.attach_through(attacher, *batches)?;
        }
        Ok(store)
    });
    let mut store = match worker {
        Ok(s) => s,
        Err(error) => {
            let message = if prepare {
                Message::Ready {
                    worker: id,
                    result: Err(error),
                }
            } else {
                Message::Done {
                    worker: id,
                    result: Err(error),
                    physical_error: None,
                    kept: None,
                    sorted: None,
                    indexed: None,
                }
            };
            drop(back.blocking_send(message));
            return;
        }
    };
    if prepare
        && back
            .blocking_send(Message::Ready {
                worker: id,
                result: Ok(()),
            })
            .is_err()
    {
        return;
    }
    let asks = back.clone();
    store.set_refill(Refill(Box::new(move |ask| {
        let message = match ask {
            Ask::Extents(n) => Message::Need { worker: id, n },
            Ask::Grown(result) => Message::Grown { worker: id, result },
        };
        asks.blocking_send(message)
            .map_err(|_| gone("ask the shard for extents"))?;
        more.recv()
            .map_err(|_| gone("take extents from the shard"))?
    })));
    while let Ok(mut job) = jobs.recv() {
        let t = Instant::now();
        let waited = store.read_wait_ns();
        store.keep_pages(job.keep.take());
        let (ran, unwound) = run_guarded(&mut store, &mut job);
        // Whatever the job's end, every run it handed the device is answered before its result
        // goes back: the shard releases a failed job's extents once it has the result, and the
        // next job starts the allocator over, so none of this job's writes may land after. The
        // job's own failure is the one reported.
        let drained = store.drain();
        // The pages kept go back however the job ended: the shard's cache lent their room.
        let kept = store.take_kept();
        // An issued failure precedes the unwind. A worker unwind is non-abandonable too,
        // so terminal cleanup must retain it even when a partial input was canceled.
        let physical_error = drained.as_ref().err().cloned().or_else(|| {
            if unwound {
                ran.as_ref().err().cloned()
            } else {
                None
            }
        });
        // A job's time: the work it did, which the pool sizes itself by.
        let result = ran.and_then(|(parts, units)| {
            drained.map(|()| Output {
                parts,
                unused: store.unused_grant(),
                end: store.end(),
                ns: ns(Instant::now().saturating_duration_since(t)),
                units,
                read_wait_ns: store.read_wait_ns().saturating_sub(waited),
                kept: None,
            })
        });
        // A sorted packing's buffers, and an index job's index, go back with its result.
        let (sorted, indexed) = match job.work {
            Work::Sort(pack) => (Some(Box::new(pack)), None),
            Work::Index(index) => (None, Some(index)),
            _ => (None, None),
        };
        let done = Message::Done {
            worker: id,
            result,
            physical_error,
            kept,
            sorted,
            indexed,
        };
        if unwound {
            // Close admission before Done: its owner can immediately try this same seat.
            // Cursors/runs that unwound did not return every loan, so never reuse the Store.
            drop(jobs);
            drop(back.blocking_send(done));
            retire(store, &back, id);
            return;
        }
        if back.blocking_send(done).is_err() {
            return;
        }
    }
    retire(store, &back, id);
}

fn retire<F: BlockFile>(store: Store<F>, back: &Sender<Message>, id: usize) {
    // Raw Drop is unacknowledged cleanup. Explicit terminal retirement must report
    // the worker attachment's lifecycle error before its return sender reaches EOF.
    // Normal close has consumed Done; after unwind a bounded sender waits for its slot.
    match store.into_file() {
        IntoFile::Finished { file, result } => {
            drop(back.blocking_send(Message::Retired { worker: id, result }));
            // EOF and the native retirement receipt still follow this file's Drop/TLS.
            drop(file);
        }
        IntoFile::Refused { owner, error } => {
            drop(back.blocking_send(Message::Retired {
                worker: id,
                result: Err(error),
            }));
            // An exceptional refusal keeps the complete owner for cold cleanup; no early
            // physical receipt or actor acknowledgement follows this message alone.
            drop(owner);
        }
    }
}

/// The branches a job made, each with its first key, in key order.
type Parts = Vec<(Vec<u8>, Branch)>;

/// A guarded job keeps the Store, job/grants and accepted transfer ledger outside unwind.
/// Only this job's nested cursor/Builder stack unwinds; its Store is then drained and retired.
fn run_guarded<F: BlockFile>(
    store: &mut Store<F>,
    job: &mut Job,
) -> (Result<(Parts, u64), Error>, bool) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(store, job))) {
        Ok(result) => (result, false),
        Err(payload) => {
            let reason = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("foreign callback unwound without a string payload");
            (
                Err(Error::Io {
                    op: "run a maintenance worker job",
                    detail: format!("maintenance worker job unwound: {reason}"),
                }),
                true,
            )
        }
    }
}

/// Runs `job`: the branches made, and the work units a compaction merged (none for a packing).
fn run<F: BlockFile>(store: &mut Store<F>, job: &mut Job) -> Result<(Parts, u64), Error> {
    store.begin_job(&job.grant, job.file_end, job.generation)?;
    let mut units = 0u64;
    let parts = match &mut job.work {
        Work::Compact(t) => {
            let mut c = Compaction::new(
                store,
                &t.inputs,
                &t.from,
                t.end.as_deref(),
                t.drop_tombstones,
                t.per,
            )?;
            units = c.step(store, &t.inputs, u64::MAX)?;
            c.finish(store)?
        }
        Work::Sort(pack) => {
            let SortPack {
                entries,
                skip,
                arena,
                sorter,
                keys,
                cuts,
                ends,
            } = pack;
            let kept = sorter.sort(arena, *skip)?;
            // The memtable counted each key once as it took it: the sort keeps as many.
            if u64::try_from(kept).ok() != Some(*entries) {
                return Err(Error::Corruption {
                    what: "a memtable's sorted entries",
                    why: crate::error::Malformed::CountMismatch,
                });
            }
            // A branch for each root pivot the memtable holds keys of: the first ends below the
            // second pivot's key, each next one below the pivot key after its own.
            sorter.ends_below(arena, cuts.get(1..).unwrap_or(&[]), ends)?;
            let end_of = |part: usize| ends.get(part).copied().unwrap_or(kept);
            // The walk's keys, each its newest entry, with their offsets in the copy, the
            // arena's own: the index over kept memtables is built from them.
            keys.clear();
            keys.hashes
                .try_reserve(usize::try_from(*entries).unwrap_or(usize::MAX))
                .map_err(|_| Error::LimitExceeded {
                    what: "a memtable's key list",
                    limit: *entries,
                })?;
            keys.offsets
                .try_reserve(usize::try_from(*entries).unwrap_or(usize::MAX))
                .map_err(|_| Error::LimitExceeded {
                    what: "a memtable's key list",
                    limit: *entries,
                })?;
            let mut parts: Vec<(Vec<u8>, Branch)> = Vec::new();
            let (mut part, mut walked) = (0usize, 0usize);
            let mut building: Option<Builder> = None;
            sorter.each_at(arena, |k, op, v, h, at| {
                // Past the part the last entry ended: its branch finished, and every part with
                // no entry passed over.
                while walked >= end_of(part) {
                    if let Some(b) = building.take() {
                        let key = cuts.get(part).cloned().unwrap_or_default();
                        parts.push((key, b.finish(store)?));
                    }
                    part = part.checked_add(1).ok_or(Error::Corruption {
                        what: "a memtable's packed parts",
                        why: crate::error::Malformed::CountMismatch,
                    })?;
                }
                if building.is_none() {
                    let n = end_of(part).saturating_sub(walked);
                    let n = u64::try_from(n).unwrap_or(u64::MAX);
                    let mut b = Builder::new(store, Keys::Exactly(n))?;
                    // The part's keys share what every key of the memtable does, and what its
                    // first shares with the next part's cut, which they all lie below.
                    let below = cuts
                        .get(part.saturating_add(1))
                        .map_or(0, |c| crate::util::keys::common_prefix(k, c));
                    b.set_shared((*skip).max(below))?;
                    building = Some(b);
                }
                let b = building.as_mut().ok_or(Error::Corruption {
                    what: "a memtable's packed parts",
                    why: crate::error::Malformed::CountMismatch,
                })?;
                keys.push(crate::branch::filter::hash(k), at);
                b.add_hashed(store, k, op, v, h)?;
                walked = walked.saturating_add(1);
                Ok(())
            })?;
            if let Some(b) = building.take() {
                let key = cuts.get(part).cloned().unwrap_or_default();
                parts.push((key, b.finish(store)?));
            }
            parts
        }
        Work::Grow(g) => {
            store.grow_segment(g.start, g.extents, g.add)?;
            Vec::new()
        }
        Work::Index(job) => {
            let IndexJob {
                index,
                lists,
                oldest,
                sweep,
            } = job.as_mut();
            index.extend(lists, *oldest, *sweep);
            Vec::new()
        }
    };
    Ok((parts, units))
}

fn gone(what: &'static str) -> Error {
    Error::Io {
        op: what,
        detail: "the other end of a maintenance worker's channel is gone".into(),
    }
}

/// A worker as the shard holds it: its channels' sending ends, its thread, whose job is out on
/// it and that job's number, and the extents topped up to that job.
#[derive(Debug)]
struct Worker {
    jobs: Option<SyncSender<Box<Job>>>,
    more: Option<SyncSender<Result<TopUp, Error>>>,
    thread: Option<JoinHandle<()>>,
    owner: Option<Owner>,
    job: u64,
    topped: Vec<u64>,
}

/// The shard's workers.
pub struct Pool {
    spawn: Spawn,
    workers: Vec<Option<Worker>>,
    /// The workers' channel back, cloned into each seat; dropped last, so the channel ends once
    /// every worker has. A runtime's channel, so that a shard owned by a runtime task can await
    /// its workers' messages; plain threads send and take on it as on a std channel.
    back: Option<Sender<Message>>,
    messages: ChannelReceiver<Message>,
    /// Jobs back for an owner other than the one taking: at most every job the owners have out
    /// (one node's compactions, the memtables' packings), since a worker freed here can take its
    /// next job before its owner takes this one.
    undelivered: VecDeque<(Owner, Back)>,
    /// Workers' asks for more extents the shard's admitted extents could not give yet, oldest
    /// first: at most one a worker, each waiting for its one reply.
    needs: VecDeque<(usize, usize)>,
    /// The growth handed to a waiting asker instead of a free worker ([`Self::send_growth`]):
    /// the asker, its ask, and the growth's ticket. Its ask waits outside `needs` until the
    /// asker reports the growth.
    growing: Option<(usize, usize, Ticket)>,
    /// Jobs handed to workers since the pool began: the next one's number ([`Ticket`]).
    handed: u64,
    /// The cores the OS reports less the shard's own: the most workers.
    most: usize,
    /// The workers Little's law measured the need for.
    want: usize,
    /// Workers' nanoseconds on jobs, and the shard's own waits on them, since the period
    /// started at `since`.
    busy_ns: u64,
    waited_ns: u64,
    since: Option<Instant>,
    /// The device's issuer for the workers started from now on, and the batches each may have
    /// out ([`Self::set_attach`]).
    attach: Option<(Attacher, usize)>,
    /// A nonblocking take found work out but no message yet: a task can await the receiver.
    waiting: bool,
    /// Messages taken from the workers since the pool began. A feeding owner that takes one
    /// after it last found every feed starved feeds again before it waits
    /// ([`ShardDb::feed_from`](crate::shard_db::ShardDb)): the message may have brought a
    /// starved feed's buffer back. Wrapping: only its change is read.
    accepted: u64,
    /// Terminal jobs are abandoned, their refills refused, before the job channels close.
    closing: bool,
    /// Every worker dropped its store and return sender, including its device attachment.
    closed: bool,
    close_error: Option<Error>,
    /// Every charged seat acknowledged startup before any runtime job was admitted.
    prepared: bool,
    /// Handles adopted on the cold caller; runtime retirement sends no native owner.
    retirement: Option<slates_rt::retirement::RetirementLease>,
}

/// A borrowed wait's share of the pool's measured period, including a request that interrupts
/// it. Dropping the future returns the measurement once without consuming any message.
struct WaitMeasure<'a> {
    waited: &'a mut u64,
    since: Instant,
}

impl Drop for WaitMeasure<'_> {
    fn drop(&mut self) {
        *self.waited = self.waited.saturating_add(ns(self.since.elapsed()));
    }
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("workers", &self.workers)
            .field("most", &self.most)
            .field("want", &self.want)
            .finish_non_exhaustive()
    }
}

impl Pool {
    /// A pool of up to `most` workers, each started by `spawn`; none until a job needs one.
    /// Refused when the runtime has no synchronization cell left for its channel back.
    pub fn new(spawn: Spawn, most: usize) -> Result<Self, Error> {
        let most = most.max(1);
        let (back, messages) = slates_rt::sync::channel(most.saturating_mul(MESSAGES_A_WORKER))
            .map_err(|error| Error::Io {
                op: "make the maintenance workers' channel back",
                detail: error.to_string(),
            })?;
        Ok(Self {
            spawn,
            workers: Vec::new(),
            back: Some(back),
            messages,
            undelivered: VecDeque::new(),
            needs: VecDeque::new(),
            growing: None,
            handed: 0,
            most,
            want: 1,
            busy_ns: 0,
            waited_ns: 0,
            since: None,
            attach: None,
            waiting: false,
            accepted: 0,
            closing: false,
            closed: false,
            close_error: None,
            prepared: false,
            retirement: None,
        })
    }

    /// Workers started from now on attach their stores to the device's issuer through
    /// `attacher`, each with up to `batches` out (`ShardDb::attach`).
    pub fn set_attach(&mut self, attacher: Attacher, batches: usize) {
        self.prepared = false;
        self.attach = Some((attacher, batches));
    }

    /// Preparation is cold: no actor task may open files, spawn threads or wait for startup.
    pub(crate) fn can_prepare(&self) -> Result<(), Error> {
        if self.closing || (!self.prepared && (!self.workers.is_empty() || self.attach.is_none())) {
            return Err(Error::InvalidArgument {
                what: "existing workers incompatible with runtime preparation",
            });
        }
        Ok(())
    }

    pub(crate) fn prepared(&self) -> bool {
        self.prepared && !self.closing && self.retirement.is_some()
    }

    /// Starts every seat already charged by capacity, then accepts its actual startup result.
    /// No job can run yet, so Ready never overlaps the feed/Need/Done channel roles.
    pub(crate) fn prepare(&mut self) -> Result<(), Error> {
        if slates_rt::registry::current_shard().is_some() {
            return Err(Error::InvalidArgument {
                what: "maintenance worker preparation inside a runtime task",
            });
        }
        self.can_prepare()?;
        if self.prepared {
            return Ok(());
        }
        for _ in 0..self.most {
            let at = self.start(true)?;
            match self
                .messages
                .blocking_recv()
                .map_err(|_| gone("take worker startup"))?
            {
                Message::Ready { worker, result } if worker == at => result?,
                _ => return Err(gone("worker startup with no matching receipt")),
            }
        }
        self.prepared = true;
        Ok(())
    }

    pub(crate) fn adopt_retirement(
        &mut self,
        mut lease: slates_rt::retirement::RetirementLease,
    ) -> Result<(), Error> {
        if slates_rt::registry::with_current(|_| ()).is_some() {
            return Err(Error::InvalidArgument {
                what: "native worker adoption inside a runtime task",
            });
        }
        if !self.prepared || self.retirement.is_some() || lease.capacity() != self.most {
            return Err(Error::InvalidArgument {
                what: "native worker adoption outside its prepared capacity",
            });
        }
        let mut threads = Vec::new();
        threads
            .try_reserve_exact(self.most)
            .map_err(|_| Error::LimitExceeded {
                what: "native maintenance retirement handles",
                limit: u64::try_from(self.most).unwrap_or(u64::MAX),
            })?;
        for worker in self.workers.iter_mut().flatten() {
            if let Some(thread) = worker.thread.take() {
                threads.push(thread);
            }
        }
        let accepted = lease.adopt(&mut threads);
        if lease.adopted() {
            // Even a receipt failure after acceptance keeps the native owner in Runtime.
            self.retirement = Some(lease);
        } else {
            let mut returned = threads.into_iter();
            for worker in self.workers.iter_mut().flatten() {
                if worker.thread.is_none() {
                    worker.thread = returned.next();
                }
            }
        }
        accepted.map_err(|error| Error::Io {
            op: "adopt native maintenance threads",
            detail: error.to_string(),
        })
    }

    pub(crate) fn retirement_owned(&self) -> bool {
        self.retirement.is_some()
    }

    pub(crate) fn clear_wait(&mut self) {
        self.waiting = false;
    }

    pub(crate) fn waiting(&self) -> bool {
        self.waiting && (self.closing || self.workers.iter().flatten().any(|w| w.owner.is_some()))
    }

    pub(crate) fn want_message(&mut self) {
        self.waiting = true;
    }

    /// Canceling this borrowed receive leaves the message on the channel. The caller routes
    /// a received message with its store before returning to the task's next request.
    pub(crate) async fn receive(&mut self) -> Result<Option<Message>, Error> {
        if self.closed
            && let Some(retirement) = &mut self.retirement
        {
            if let Err(error) = retirement.wait().await {
                // A matching failed join still proves retirement. Route it through
                // close_paced so a prior physical failure remains the first error.
                if matches!(error, slates_rt::RtError::NotOnShardThread)
                    || !matches!(retirement.try_result(), Ok(Some(Err(_))))
                {
                    return Err(Error::Io {
                        op: "retire native maintenance threads",
                        detail: error.to_string(),
                    });
                }
                self.close_error.get_or_insert(Error::Io {
                    op: "join native maintenance threads",
                    detail: error.to_string(),
                });
            }
            self.waiting = false;
            return Ok(None);
        }
        let _measure = WaitMeasure {
            waited: &mut self.waited_ns,
            since: Instant::now(),
        };
        match self.messages.recv().await {
            Ok(message) => Ok(Some(message)),
            Err(slates_rt::sync::SyncError::Closed(())) if self.closing && self.back.is_none() => {
                self.closed = true;
                self.waiting = false;
                Ok(None)
            }
            Err(error) => Err(Error::Io {
                op: "take a worker's message",
                detail: format!("{error:?}"),
            }),
        }
    }

    /// Closing a partial feed makes its worker finish that input; no new extent grant is
    /// made after this point. Existing grants stay owned until every job is physically drained.
    pub(crate) fn begin_close(&mut self) {
        self.closing = true;
        for (_, back) in &mut self.undelivered {
            if let Some(error) = back.physical_error.take() {
                self.close_error.get_or_insert(error);
            }
        }
        // An ask that waited for growth is refused now, so its worker ends its job.
        while let Some((worker, _)) = self.needs.pop_front() {
            let refused = self
                .workers
                .get(worker)
                .and_then(Option::as_ref)
                .and_then(|w| w.more.as_ref())
                .map(|more| more.try_send(Err(gone("give a closing worker extents"))));
            if let Some(Err(std::sync::mpsc::TrySendError::Disconnected(_))) = refused {
                self.close_error
                    .get_or_insert(gone("give a closing worker extents"));
            }
        }
    }

    pub(crate) fn note_close_error(&mut self, error: Error) {
        self.close_error.get_or_insert(error);
    }

    /// Takes terminal results without applying their unfinished outputs, then closes the
    /// idle workers' channels. False awaits the same return receiver, including its final EOF.
    pub(crate) fn close_paced<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
    ) -> Result<bool, Error> {
        self.begin_close();
        loop {
            match self.next(store, false) {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    self.close_error.get_or_insert(error);
                    break;
                }
            }
        }
        if self.workers.iter().flatten().any(|w| w.owner.is_some()) {
            self.waiting = true;
            return Ok(false);
        }
        // Done is sent after the worker Store drained every transfer. Closing its job
        // receiver now ends the loop; EOF follows its Store/attachment destruction.
        for worker in self.workers.iter_mut().flatten() {
            worker.jobs = None;
            worker.more = None;
        }
        self.back = None;
        if !self.closed {
            match self.next(store, false) {
                Ok(_) => {}
                Err(error) => {
                    self.close_error.get_or_insert(error);
                }
            }
        }
        if self.closed {
            if let Some(retirement) = &mut self.retirement {
                retirement.request().map_err(|error| Error::Io {
                    op: "signal native maintenance retirement",
                    detail: error.to_string(),
                })?;
                match retirement.try_result().map_err(|error| Error::Io {
                    op: "take native maintenance retirement",
                    detail: error.to_string(),
                })? {
                    None => {
                        self.waiting = true;
                        return Ok(false);
                    }
                    Some(Err(error)) => {
                        self.close_error.get_or_insert(Error::Io {
                            op: "join native maintenance threads",
                            detail: error.to_string(),
                        });
                    }
                    Some(Ok(())) => {}
                }
            }
            return self.close_error.take().map_or(Ok(true), Err);
        }
        self.waiting = true;
        Ok(false)
    }

    /// The same terminal state for a plain thread: each unfinished pass takes one worker
    /// receipt, waiting for it. An error is reported only after the channel's terminal EOF.
    pub(crate) fn close_blocking<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
    ) -> Result<(), Error> {
        if slates_rt::registry::with_current(|_| ()).is_some() {
            return Err(Error::InvalidArgument {
                what: "blocking maintenance retirement inside a runtime task",
            });
        }
        while !self.close_paced(store)? {
            if self.closed
                && let Some(retirement) = &mut self.retirement
            {
                if let Err(error) = retirement.wait_blocking() {
                    self.close_error.get_or_insert(Error::Io {
                        op: "retire native maintenance threads",
                        detail: error.to_string(),
                    });
                }
                continue;
            }
            if let Err(error) = self.next(store, true) {
                self.close_error.get_or_insert(error);
            }
        }
        Ok(())
    }

    /// The cores the OS reports less the shard's own thread, at least one.
    pub fn cores() -> usize {
        std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .saturating_sub(1)
            .max(1)
    }

    /// The admitted worker maximum, including workers that may start in a later period.
    pub(crate) fn capacity(&self) -> usize {
        self.most
    }

    /// Warm worker seats held, each charged until its terminal Store/attachment retirement.
    pub fn workers(&self) -> usize {
        self.workers.iter().filter(|w| w.is_some()).count()
    }

    /// Workers a job handed now could have beside those busy: the cores the pool may use less
    /// the jobs out, at least one (the job's own).
    pub fn idle(&self) -> usize {
        self.most.saturating_sub(self.held()).max(1)
    }

    /// The active workers the measured need asks for; idle warm seats may exceed it.
    pub fn want(&self) -> usize {
        self.want
    }

    /// A period starts: the last one's measure sets the workers wanted (the module's rule).
    /// With no shard time between two periods (a drain), every core is wanted.
    pub fn period_started(&mut self) {
        let now = Instant::now();
        if let Some(since) = self.since {
            let wall = ns(now.saturating_duration_since(since));
            let period = wall.saturating_sub(self.waited_ns);
            self.want = if period == 0 {
                self.most
            } else {
                usize::try_from(crate::util::div_ceil(self.busy_ns, period).unwrap_or(0))
                    .unwrap_or(usize::MAX)
                    .max(1)
                    .min(self.most)
            };
        }
        self.since = Some(now);
        self.busy_ns = 0;
        self.waited_ns = 0;
        // Idle seats stay warm and charged. Dropping one here could leave its Store alive
        // behind device detach while a replacement reused the same admitted seat.
    }

    /// Whether a job handed now would go out: a worker is free, or another may start, within
    /// the measured need or, when `waiting` (the shard would otherwise wait on a job no worker
    /// takes, itself evidence the need is more), within the cores.
    ///
    /// A worker is free once its result is sent, not once its owner takes it, so what the
    /// workers sent is taken first, as [`Self::take`] takes it: asks for extents answered from
    /// `store`, results kept for their owners. Counted held until taken, a seat stays held by a
    /// result whose owner takes it only after the refused work is done: the kept index's,
    /// taken back once the trunk is idle, held the one seat the trunk's measured need allowed,
    /// and the trunk, refused it, never became idle (mantle's ranges bench, `--no-inline`,
    /// 2026-10-14).
    pub fn can_take<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        waiting: bool,
    ) -> Result<bool, Error> {
        if self.closing {
            return Ok(false);
        }
        let bound = self.bound(waiting);
        // The channel holds at most this many: each pass takes one or ends.
        for _ in 0..self.most.saturating_mul(MESSAGES_A_WORKER) {
            if self.held() < bound || !self.next(store, false)? {
                break;
            }
        }
        Ok(self.held() < bound)
    }

    /// The workers [`Self::can_take`] allows busy: the cores when `waiting`, else the measured
    /// need.
    fn bound(&self, waiting: bool) -> usize {
        if waiting { self.most } else { self.want }
    }

    /// Workers with a job out whose result is not taken yet.
    fn held(&self) -> usize {
        self.workers
            .iter()
            .flatten()
            .filter(|w| w.owner.is_some())
            .count()
    }

    /// Hands `job` to a free worker for `owner`, starting one if none is free within the bound
    /// [`Self::can_take`] states for `waiting`: the job's ticket, which its result carries back,
    /// or the job back when every worker is busy. Every hard refusal returns its owned job too,
    /// before any grant is lost.
    /// The growth handed to `worker` when its job ended or it retired without reporting it
    /// (it unwound in the segment's write, or its report could not be sent): the growth comes
    /// back under its ticket as failed, with `cause`, the worker's own failure, when it has one,
    /// so whoever takes the growth's result sees the cause the close reports; the shard takes it
    /// as it takes a growth job's failure, its booking left pending to be booked again.
    fn growth_unreported(&mut self, worker: usize, cause: Option<Error>) {
        let Some((_, _, ticket)) = self.growing.take_if(|(asker, _, _)| *asker == worker) else {
            return;
        };
        let error = cause.unwrap_or_else(|| Error::Io {
            op: "grow the shard's file on a worker waiting for extents",
            detail: "the worker ended before it reported the growth".into(),
        });
        self.close_error.get_or_insert_with(|| error.clone());
        self.undelivered.push_back((
            Owner::Grow,
            Back {
                ticket,
                result: Err(error.clone()),
                kept: None,
                physical_error: Some(error),
                topped: Vec::new(),
                sorted: None,
                indexed: None,
            },
        ));
    }

    /// Hands `job`, a growth of the shard's file, to a worker free within the cores; with none
    /// free and an ask for extents waiting for growth, to the oldest such asker as its reply
    /// ([`TopUp::Grow`]), which adds and writes the segment whole and reports it
    /// ([`Message::Grown`]) before its ask is answered. A worker waiting for extents holds its
    /// seat, so when every seat's job waits for extents the file still grows, by a worker that
    /// would otherwise wait. The ticket names the growth's result as [`Self::send`]'s does; the
    /// job back when no worker can take it.
    pub fn send_growth<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        job: Box<Job>,
    ) -> Result<Result<Ticket, Box<Job>>, (Error, Box<Job>)> {
        // As [`Self::send`] admits: a runtime's pool hands out nothing before its preparation,
        // and nothing is taken from the workers or answered first.
        if slates_rt::registry::current_shard().is_some()
            && !self.prepared()
            && !crate::store::blocking_permitted()
        {
            return Err((
                Error::InvalidArgument {
                    what: "runtime worker admission before preparation",
                },
                job,
            ));
        }
        match self.can_take(store, true) {
            Ok(true) => return self.send(job, Owner::Grow, true),
            Ok(false) => {}
            Err(error) => return Err((error, job)),
        }
        if self.closing || self.growing.is_some() {
            return Ok(Err(job));
        }
        let Some(&(asker, n)) = self.needs.front() else {
            return Ok(Err(job));
        };
        let Work::Grow(growth) = &job.work else {
            return Err((
                Error::InvalidArgument {
                    what: "a growth handed out that is not one",
                },
                job,
            ));
        };
        let reply = TopUp::Grow {
            start: growth.start,
            extents: growth.extents,
            add: growth.add,
        };
        let Some(next) = self.handed.checked_add(1) else {
            return Err((
                Error::InvalidArgument {
                    what: "a maintenance job past the pool's job numbers",
                },
                job,
            ));
        };
        let Some(more) = self
            .workers
            .get(asker)
            .and_then(Option::as_ref)
            .and_then(|w| w.more.as_ref())
        else {
            return Err((gone("hand a waiting worker its growth"), job));
        };
        match more.try_send(Ok(reply)) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                return Err((
                    Error::InvalidArgument {
                        what: "a maintenance worker's top-up still pending",
                    },
                    job,
                ));
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                return Err((gone("hand a waiting worker its growth"), job));
            }
        }
        self.needs.pop_front();
        // Numbered as `send` numbers a job: the number not yet handed, then the next.
        let ticket = Ticket {
            worker: asker,
            job: self.handed,
        };
        self.handed = next;
        self.growing = Some((asker, n, ticket));
        Ok(Ok(ticket))
    }

    pub fn send(
        &mut self,
        job: Box<Job>,
        owner: Owner,
        waiting: bool,
    ) -> Result<Result<Ticket, Box<Job>>, (Error, Box<Job>)> {
        if slates_rt::registry::current_shard().is_some()
            && !self.prepared()
            && !crate::store::blocking_permitted()
        {
            return Err((
                Error::InvalidArgument {
                    what: "runtime worker admission before preparation",
                },
                job,
            ));
        }
        if self.closing {
            return Err((gone("hand a closing worker its job"), job));
        }
        let Some(next) = self.handed.checked_add(1) else {
            return Err((
                Error::InvalidArgument {
                    what: "a maintenance job past the pool's job numbers",
                },
                job,
            ));
        };
        if self.held() >= self.bound(waiting) {
            return Ok(Err(job));
        }
        let free = self
            .workers
            .iter()
            .position(|w| w.as_ref().is_some_and(|w| w.owner.is_none()));
        let at = match free {
            Some(at) => at,
            None if self.workers() < self.most => match self.start(false) {
                Ok(at) => at,
                Err(error) => return Err((error, job)),
            },
            None => return Ok(Err(job)),
        };
        let Some(w) = self.workers.get_mut(at).and_then(Option::as_mut) else {
            return Err((gone("hand a worker its job"), job));
        };
        let Some(jobs) = w.jobs.as_ref() else {
            return Err((gone("hand a closing worker its job"), job));
        };
        match jobs.try_send(job) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(job)) => return Ok(Err(job)),
            Err(std::sync::mpsc::TrySendError::Disconnected(job)) => {
                return Err((gone("hand a worker its job"), job));
            }
        }
        w.owner = Some(owner);
        w.job = self.handed;
        w.topped.clear();
        let ticket = Ticket {
            worker: at,
            job: self.handed,
        };
        self.handed = next;
        Ok(Ok(ticket))
    }

    fn start(&mut self, prepare: bool) -> Result<usize, Error> {
        if slates_rt::registry::current_shard().is_some() && !crate::store::blocking_permitted() {
            return Err(Error::InvalidArgument {
                what: "maintenance worker startup inside a runtime task",
            });
        }
        let at = self
            .workers
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.workers.len());
        // Each channel holds what one worker can have waiting: one job, one top-up.
        let (jobs, job_rx) = sync_channel(1);
        let (more, more_rx) = sync_channel(1);
        let back = self.back.clone().ok_or(gone("start a worker"))?;
        let seat = Seat {
            id: at,
            jobs: job_rx,
            more: more_rx,
            back,
            attach: self.attach.clone(),
            prepare,
        };
        let thread = (self.spawn)(seat)?;
        let w = Worker {
            jobs: Some(jobs),
            more: Some(more),
            thread: Some(thread),
            owner: None,
            job: 0,
            topped: Vec::new(),
        };
        match self.workers.get_mut(at) {
            Some(slot) => *slot = Some(w),
            None => self.workers.push(Some(w)),
        }
        Ok(at)
    }

    /// The next job back for `owner`, waiting for one when `wait` and one of its jobs is out,
    /// the wait counted against the shard's period. Meanwhile it answers every worker that asks
    /// for more extents from `store`, and keeps what comes for others for them.
    pub fn take<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        owner: Owner,
        wait: bool,
    ) -> Result<Option<Back>, Error> {
        blocking_wait_allowed(wait)?;
        loop {
            if let Some(at) = self.undelivered.iter().position(|(o, _)| *o == owner)
                && let Some((_, back)) = self.undelivered.swap_remove_back(at)
            {
                return Ok(Some(back));
            }
            if !self.next(store, wait && self.out_for(owner))? {
                // Compactions and index jobs own their inputs: their owner waits for the result,
                // the range's task on the pool's channel rather than polling. Packing's caller
                // marks its own wait.
                if matches!(owner, Owner::Trunk | Owner::Index) && self.out_for(owner) {
                    self.waiting = true;
                }
                return Ok(None);
            }
        }
    }

    /// Takes one message from the workers, waiting for one when `wait`: an ask for extents is
    /// answered from `store`, a result kept for its owner. Whether one was taken.
    fn next<F: BlockFile>(&mut self, store: &mut Store<F>, wait: bool) -> Result<bool, Error> {
        let message = match self.messages.try_recv() {
            Ok(Some(m)) => m,
            Ok(None) if wait => {
                let t = Instant::now();
                let m = match self.messages.blocking_recv() {
                    Ok(message) => message,
                    Err(_) if self.closing && self.back.is_none() => {
                        self.closed = true;
                        self.waiting = false;
                        return Ok(false);
                    }
                    Err(_) => return Err(gone("take a worker's message")),
                };
                let waited = ns(Instant::now().saturating_duration_since(t));
                self.waited_ns = self.waited_ns.saturating_add(waited);
                m
            }
            Ok(None) => return Ok(false),
            Err(_) if self.closing && self.back.is_none() => {
                self.closed = true;
                self.waiting = false;
                return Ok(false);
            }
            Err(_) => return Err(gone("take a worker's message")),
        };
        self.accept(store, message)
    }

    pub(crate) fn accept<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        message: Message,
    ) -> Result<bool, Error> {
        self.waiting = false;
        self.accepted = self.accepted.wrapping_add(1);
        match message {
            Message::Ready { result, .. } => result?,
            Message::Retired { worker, result } => {
                if self.workers.get(worker).and_then(Option::as_ref).is_none() {
                    return Err(gone("take a worker's terminal result"));
                }
                self.growth_unreported(worker, result.as_ref().err().cloned());
                if let Err(error) = result {
                    self.close_error.get_or_insert(error);
                }
            }
            Message::Need { worker, n } => {
                // A file grown by segments may not have admitted the extents yet: the ask waits,
                // one a worker, until growth does ([`Self::answer_needs`]).
                let asked = u64::try_from(n).unwrap_or(u64::MAX);
                if !self.closing && store.segments().is_some() && store.capacity() < asked {
                    if store.grows_in_place() {
                        // Where the shard may wait, its file grows by the ask here.
                        store.grow_now(asked)?;
                    }
                    if store.capacity() < asked {
                        self.needs.push_back((worker, n));
                        return Ok(true);
                    }
                }
                let more = if self.closing {
                    Err(gone("give a closing worker extents"))
                } else {
                    store.grant(n)
                };
                let w = self
                    .workers
                    .get_mut(worker)
                    .and_then(Option::as_mut)
                    .ok_or(gone("give a worker extents"))?;
                if let Ok(extents) = &more {
                    w.topped.extend_from_slice(extents);
                }
                // One Need waits for its one reply. A refused reply still belongs to
                // w.topped until Done proves physical quiescence; never wait in the owner.
                match w
                    .more
                    .as_ref()
                    .ok_or(gone("give a closing worker extents"))?
                    .try_send(more.map(TopUp::Extents))
                {
                    Ok(()) => {}
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        return Err(Error::InvalidArgument {
                            what: "a maintenance worker's top-up still pending",
                        });
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                        return Err(gone("give a worker extents"));
                    }
                }
            }
            Message::Grown { worker, result } => {
                let Some((asker, n, ticket)) = self.growing.take() else {
                    return Err(Error::InvalidArgument {
                        what: "a growth reported by a worker handed none",
                    });
                };
                if asker != worker {
                    return Err(Error::InvalidArgument {
                        what: "a growth reported by a worker handed none",
                    });
                }
                // A failed segment write is the pool's first cause from here, whatever takes
                // the growth's result or refuses the ask after it.
                let physical_error = result.as_ref().err().cloned();
                if let Some(error) = &physical_error {
                    self.close_error.get_or_insert_with(|| error.clone());
                }
                // The growth comes back to the file's grower as a growth job's result would:
                // nothing made, no grant, its time the asker's own job's.
                self.undelivered.push_back((
                    Owner::Grow,
                    Back {
                        ticket,
                        result: result.map(|()| Output {
                            parts: Vec::new(),
                            unused: Vec::new(),
                            end: store.end(),
                            ns: 0,
                            units: 0,
                            read_wait_ns: 0,
                            kept: None,
                        }),
                        kept: None,
                        physical_error,
                        topped: Vec::new(),
                        sorted: None,
                        indexed: None,
                    },
                ));
                if self.closing {
                    // Closing refused the asks that waited; this one is refused now, as the
                    // worker waits for one more reply.
                    let refused = self
                        .workers
                        .get(asker)
                        .and_then(Option::as_ref)
                        .and_then(|w| w.more.as_ref())
                        .map(|more| more.try_send(Err(gone("give a closing worker extents"))));
                    if let Some(Err(std::sync::mpsc::TrySendError::Disconnected(_))) = refused {
                        self.close_error
                            .get_or_insert(gone("give a closing worker extents"));
                    }
                } else {
                    // Answered first once the growth is admitted ([`Self::answer_needs`]).
                    self.needs.push_front((asker, n));
                }
            }
            Message::Done {
                worker,
                result,
                physical_error,
                kept,
                sorted,
                indexed,
            } => {
                // A canceled request can consume Done before closing starts. Physical
                // failures and worker unwinds stay terminal even after that Back is taken.
                if let Some(error) = &physical_error {
                    self.close_error.get_or_insert_with(|| error.clone());
                }
                let cause = physical_error
                    .clone()
                    .or_else(|| result.as_ref().err().cloned());
                self.growth_unreported(worker, cause);
                let w = self
                    .workers
                    .get_mut(worker)
                    .and_then(Option::as_mut)
                    .ok_or(gone("take a worker's result"))?;
                let of = w.owner.take().ok_or(Error::InvalidArgument {
                    what: "a maintenance worker's result for no job",
                })?;
                let ticket = Ticket { worker, job: w.job };
                let topped = std::mem::take(&mut w.topped);
                if let Ok(out) = &result {
                    self.busy_ns = self.busy_ns.saturating_add(out.ns);
                }
                self.undelivered.push_back((
                    of,
                    Back {
                        ticket,
                        result,
                        kept,
                        physical_error,
                        topped,
                        sorted: sorted.map(|pack| *pack),
                        indexed,
                    },
                ));
            }
        }
        Ok(true)
    }

    /// Waits for one message from the workers, answered or kept as [`Self::take`] does: false,
    /// with nothing taken, when no worker has a job out to send one.
    pub fn wait_any<F: BlockFile>(&mut self, store: &mut Store<F>) -> Result<bool, Error> {
        blocking_wait_allowed(true)?;
        if !self.workers.iter().flatten().any(|w| w.owner.is_some()) {
            return Ok(false);
        }
        self.next(store, true)
    }

    /// Messages taken from the workers since the pool began, wrapping: only its change is read.
    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    /// Whether a job of `owner`'s is out, or back and not yet taken.
    /// Answers the asks for extents that waited, oldest first, while the store's admitted
    /// extents give them: after the file grows, or extents are freed.
    pub fn answer_needs<F: BlockFile>(&mut self, store: &mut Store<F>) -> Result<(), Error> {
        while let Some(&(worker, n)) = self.needs.front() {
            let asked = u64::try_from(n).unwrap_or(u64::MAX);
            if store.capacity() < asked {
                return Ok(());
            }
            self.needs.pop_front();
            let more = store.grant(n);
            let w = self
                .workers
                .get_mut(worker)
                .and_then(Option::as_mut)
                .ok_or(gone("give a worker extents"))?;
            if let Ok(extents) = &more {
                w.topped.extend_from_slice(extents);
            }
            match w
                .more
                .as_ref()
                .ok_or(gone("give a closing worker extents"))?
                .try_send(more.map(TopUp::Extents))
            {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    return Err(Error::InvalidArgument {
                        what: "a maintenance worker's top-up still pending",
                    });
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    return Err(gone("give a worker extents"));
                }
            }
        }
        Ok(())
    }

    /// The extents the waiting asks want in all: what the shard's file grows by at least.
    pub fn needs_wanted(&self) -> u64 {
        self.needs
            .iter()
            .map(|&(_, n)| u64::try_from(n).unwrap_or(u64::MAX))
            .fold(0u64, u64::saturating_add)
    }

    pub fn out_for(&self, owner: Owner) -> bool {
        self.workers
            .iter()
            .flatten()
            .any(|w| w.owner == Some(owner))
            || self.undelivered.iter().any(|(o, _)| *o == owner)
            || (owner == Owner::Grow && self.growing.is_some())
    }
}

impl Drop for Pool {
    /// Every worker's channels close and its thread is joined: a job still out runs to its end
    /// first and a top-up it waits for is refused, so each worker ends; the messages they send
    /// meanwhile are taken until the last worker's end closes the channel back.
    fn drop(&mut self) {
        if let Some(retirement) = &mut self.retirement {
            // Cold adoption removed every handle from these seats. During exceptional
            // service Drop, closing both producer ends then dropping messages lets any
            // blocked worker publication fail; the independent owner still joins it.
            self.workers.clear();
            self.back = None;
            if slates_rt::registry::with_current(|_| ()).is_none() {
                while self.messages.blocking_recv().is_ok() {}
                let _ = retirement.wait_blocking();
            } else {
                let _ = retirement.request();
            }
            return;
        }
        // Standalone cold Pools retain their explicit blocking ownership contract.
        let threads: Vec<JoinHandle<()>> = self
            .workers
            .iter_mut()
            .filter_map(|w| w.take().and_then(|mut w| w.thread.take()))
            .collect();
        self.back = None;
        while self.messages.blocking_recv().is_ok() {}
        for t in threads {
            drop(t.join());
        }
    }
}

/// Borrowed blocking APIs refuse before consuming a ready receipt or buffer. Runtime
/// callers keep the same job and await its existing receiver through the paced owner.
fn blocking_wait_allowed(wait: bool) -> Result<(), Error> {
    if wait && slates_rt::registry::current_shard().is_some() && !crate::store::blocking_permitted()
    {
        Err(Error::InvalidArgument {
            what: "a synchronous maintenance worker wait inside a runtime task",
        })
    } else {
        Ok(())
    }
}

/// A duration's nanoseconds, saturating.
fn ns(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "retirement_first_error_native.rs"]
mod retirement_first_error_native;

#[cfg(test)]
#[path = "seat_freed.rs"]
mod seat_freed;

#[cfg(test)]
#[path = "growth_asker.rs"]
mod growth_asker;

#[cfg(test)]
#[path = "relay.rs"]
pub(crate) mod relay;
