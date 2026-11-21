//! Measuring a device through the same file layer mantle stores data with.
//!
//! A [`Job`] issues fixed-size transfers against a [`DeviceFile`] at a fixed depth (transfers
//! in flight at once), bounded by an operation count and, when it has one, a wall-clock
//! budget, and records each transfer's latency. Depth is kept by a [`Pool`] of blocking
//! workers issuing positional I/O, the portable path mantle's I/O takes where the platform has
//! no asynchronous interface the device can use; a measurement describes the path mantle will
//! use, not the device in the abstract (docs/design/measurement.md §8).
//!
//! A pool's workers are started once and reused across jobs: each has a channel of its own
//! that its job arrives on, and is woken alone; none waits at a latch, because a worker that
//! has not received a job does nothing. Its threads are drawn from the process's budget
//! (`hyper_block::threads`) before any starts, and a pool grows only as deep as a job asks, so a
//! ladder of depths never runs more workers than its deepest step. Each completion samples the
//! transfers in flight, and a job reports the depth it achieved beside the depth it asked for,
//! as fio advises (research/26 §2.1).
//!
//! Write payloads are pseudo-random, and every block a job writes differs from every other:
//! some devices compress or deduplicate, and zero or repeated buffers would measure that
//! instead of the medium. Each write's blocks are stamped afresh before it is timed, so making
//! the payload costs nothing the device is charged for (audit P09).
//!
//! A job's time runs from when its workers begin work to when the last ends: starting the
//! pool's threads is outside it (audit P09).

use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{Builder, Scope, ScopedJoinHandle};
use std::time::{Duration, Instant};

use crate::histogram::Histogram;
use hyper_block::DiskError;
use hyper_block::buf::AlignedBuf;
use hyper_block::file::DeviceFile;
use hyper_block::threads::{self, Reservation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    SequentialRead,
    SequentialWrite,
    RandomRead,
    RandomWrite,
}

impl Pattern {
    fn writes(self) -> bool {
        matches!(self, Self::SequentialWrite | Self::RandomWrite)
    }

    fn random(self) -> bool {
        matches!(self, Self::RandomRead | Self::RandomWrite)
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub pattern: Pattern,
    /// Bytes per transfer; a multiple of the file's alignment.
    pub block: usize,
    /// Transfers in flight at once: at least one, and no more than the pool may hold.
    pub depth: usize,
    /// The job touches bytes `[base, base + span)` of the file: `base` aligned, `span` at
    /// least one block.
    pub base: u64,
    pub span: u64,
    /// How long the job may issue transfers. `None` bounds it by `max_ops` alone, for a job
    /// whose every transfer must happen, as a fill's must; a deadline past the last instant the
    /// clock holds is none.
    pub budget: Option<Duration>,
    pub max_ops: u64,
    /// Flush after every write, so each recorded latency is that of a durable write.
    pub sync_each: bool,
    pub seed: u64,
}

#[derive(Debug, Clone)]
pub struct JobResult {
    pub ops: u64,
    pub bytes: u64,
    pub elapsed: Duration,
    /// Per-transfer latency in nanoseconds.
    pub latency: Histogram,
    /// The depth asked for.
    pub depth: usize,
    /// Transfers in flight, sampled at each completion: their mean, and the most seen.
    pub achieved: f64,
    pub achieved_max: usize,
}

impl JobResult {
    pub fn bytes_per_sec(&self) -> f64 {
        rate(self.bytes, self.elapsed)
    }

    pub fn ops_per_sec(&self) -> f64 {
        rate(self.ops, self.elapsed)
    }
}

// Rates are floating point; u64 -> f64 rounds above 2^53, far beyond any count a bounded
// job produces.
fn rate(count: u64, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0.0;
    }
    count as f64 / secs
}

/// SplitMix64 (Steele, Lea and Flood, "Fast Splittable Pseudorandom Number Generators",
/// OOPSLA 2014): statistically sound for choosing offsets and filling payloads, and one
/// line of state per thread.
#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `[0, bound)`; zero when `bound` is zero. Lemire's multiply-shift
    /// ("Fast Random Integer Generation in an Interval", ACM TOMACS 2019) without the
    /// rejection step: the bias is below bound / 2^64, immaterial for offsets.
    pub fn below(&mut self, bound: u64) -> u64 {
        let wide = u128::from(self.next_u64()).saturating_mul(u128::from(bound));
        u64::try_from(wide >> 64).unwrap_or(0)
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let word = self.next_u64().to_le_bytes();
            for (dst, src) in chunk.iter_mut().zip(word.iter()) {
                *dst = *src;
            }
        }
    }
}

/// Runs `job` against `file` on a pool of its own depth, and returns what it measured.
pub fn run(file: &DeviceFile, job: &Job) -> Result<JobResult, DiskError> {
    with_pool(file, job.depth.max(1), |pool| pool.run(job))
}

/// What a job hands a worker.
struct Work {
    job: Job,
    block: u64,
    slots: u64,
    seed: u64,
}

/// A worker's answer to one job.
type Done = Result<Worked, DiskError>;

/// What every worker of a pool reads and counts, reset between jobs while the workers wait.
#[derive(Default)]
struct Counters {
    /// When the first worker began the job, which starts its time and its budget.
    began: Mutex<Option<Instant>>,
    /// The next sequential slot.
    next: AtomicU64,
    /// Operations claimed so far, across workers.
    issued: AtomicU64,
    /// Transfers in flight now.
    in_flight: AtomicUsize,
}

/// A pool of blocking workers that keeps a job's depth in flight against one file
/// (docs/design/measurement.md §8). It lives within [`with_pool`], whose scope joins its
/// threads, and starts a worker only when a job asks for more than it holds.
pub struct Pool<'scope, 'env> {
    scope: &'scope Scope<'scope, 'env>,
    file: &'env DeviceFile,
    counters: &'env Counters,
    /// Workers the pool may hold: what its owner sized it to, before any started.
    most: usize,
    workers: Vec<(SyncSender<Work>, ScopedJoinHandle<'scope, ()>)>,
    answers: Receiver<Done>,
    answer: SyncSender<Done>,
    reserved: Vec<Reservation>,
}

/// Runs `f` with a pool for `file` that may hold up to `most` workers; every worker it started
/// is stopped and joined before this returns.
pub fn with_pool<R>(
    file: &DeviceFile,
    most: usize,
    f: impl FnOnce(&mut Pool<'_, '_>) -> Result<R, DiskError>,
) -> Result<R, DiskError> {
    if most == 0 {
        return Err(invalid("a pool needs room for one worker"));
    }
    let counters = Counters::default();
    std::thread::scope(|scope| {
        // Each worker has at most one answer out, so `most` answers fill the channel.
        let (answer, answers) = sync_channel(most);
        let mut pool = Pool {
            scope,
            file,
            counters: &counters,
            most,
            workers: Vec::new(),
            answers,
            answer,
            reserved: Vec::new(),
        };
        let result = f(&mut pool);
        let stopped = pool.stop();
        result.and_then(|r| stopped.map(|()| r))
    })
}

impl Pool<'_, '_> {
    /// Workers started.
    pub fn workers(&self) -> usize {
        self.workers.len()
    }

    /// Starts workers until the pool holds `depth`, drawing them from the process's thread
    /// budget first: a depth past the pool's size or the budget starts none.
    pub fn grow(&mut self, depth: usize) -> Result<(), DiskError> {
        if depth > self.most {
            return Err(DiskError::Threads {
                path: self.file.path().to_path_buf(),
                asked: depth,
                left: self.most,
                ceiling: self.most,
            });
        }
        let more = depth.saturating_sub(self.workers.len());
        if more == 0 {
            return Ok(());
        }
        self.reserved
            .push(threads::reserve(more, self.file.path())?);
        for _ in 0..more {
            let (jobs, work) = sync_channel::<Work>(1);
            let (file, counters, answer) = (self.file, self.counters, self.answer.clone());
            let handle = Builder::new()
                .name("mantle-measure".into())
                .spawn_scoped(self.scope, move || serve(file, counters, &work, &answer))
                .map_err(|source| DiskError::Io {
                    op: "start a measurement worker",
                    path: file.path().to_path_buf(),
                    source,
                })?;
            self.workers.push((jobs, handle));
        }
        Ok(())
    }

    /// Runs `job` on the first `job.depth` workers, starting those the pool lacks.
    pub fn run(&mut self, job: &Job) -> Result<JobResult, DiskError> {
        let align = self.file.alignment();
        let block = u64::try_from(job.block).map_err(|_| invalid("block size"))?;
        if job.block == 0 || !align.is_aligned(job.block) {
            return Err(invalid(
                "block size must be a non-zero multiple of the alignment",
            ));
        }
        if job.depth == 0 {
            return Err(invalid("depth must be at least one"));
        }
        // Whole blocks only: the last transfer must end inside the span.
        let slots = job.span.checked_div(block).unwrap_or(0);
        if slots == 0 {
            return Err(invalid("span must hold at least one block"));
        }
        if !align.is_aligned_u64(job.base) || job.base.checked_add(job.span).is_none() {
            return Err(invalid(
                "base must be aligned, and the span end within the file's range",
            ));
        }
        if job.budget.is_none() && job.max_ops == u64::MAX {
            return Err(invalid("a job needs a budget or an operation count"));
        }
        self.grow(job.depth)?;
        // Every worker waits for its next job, so nothing reads the counters as they reset.
        *self.counters.began.lock().map_err(|_| unwound())? = None;
        self.counters.next.store(0, Ordering::Relaxed);
        self.counters.issued.store(0, Ordering::Relaxed);
        self.counters.in_flight.store(0, Ordering::Relaxed);
        let mut sent = 0usize;
        for (worker, (jobs, _)) in self.workers.iter().take(job.depth).enumerate() {
            let work = Work {
                job: job.clone(),
                block,
                slots,
                seed: job.seed ^ u64::try_from(worker).unwrap_or(0).rotate_left(32),
            };
            if jobs.send(work).is_err() {
                break;
            }
            sent = sent.saturating_add(1);
        }
        let mut latency = Histogram::new();
        let (mut ops, mut samples, mut sampled) = (0u64, 0u64, 0u64);
        let (mut achieved_max, mut ended) = (0usize, None::<Instant>);
        let mut failed = None;
        // One answer from each worker sent a job: each answers once, whatever its job did.
        for _ in 0..sent {
            match self.answers.recv().map_err(|_| unwound())? {
                Ok(worked) => {
                    ops = ops.saturating_add(worked.ops);
                    latency.merge(&worked.latency);
                    ended = ended.max(Some(worked.ended));
                    samples = samples.saturating_add(worked.ops);
                    sampled = sampled.saturating_add(worked.in_flight);
                    achieved_max = achieved_max.max(worked.most_in_flight);
                }
                Err(e) => failed = failed.or(Some(e)),
            }
        }
        if let Some(e) = failed {
            return Err(e);
        }
        if sent < job.depth {
            return Err(unwound());
        }
        let began = *self.counters.began.lock().map_err(|_| unwound())?;
        let elapsed = match (began, ended) {
            (Some(began), Some(ended)) => ended.saturating_duration_since(began),
            _ => Duration::ZERO,
        };
        Ok(JobResult {
            ops,
            bytes: ops.saturating_mul(block),
            elapsed,
            latency,
            depth: job.depth,
            // Counts of a bounded job are far below 2^53.
            achieved: if samples == 0 {
                0.0
            } else {
                sampled as f64 / samples as f64
            },
            achieved_max,
        })
    }

    /// Stops every worker and joins it: a worker ends when its channel closes.
    fn stop(&mut self) -> Result<(), DiskError> {
        let mut unwound_any = false;
        for (jobs, handle) in self.workers.drain(..) {
            drop(jobs);
            unwound_any |= handle.join().is_err();
        }
        self.reserved.clear();
        if unwound_any { Err(unwound()) } else { Ok(()) }
    }
}

/// A worker: takes each job from its own channel, runs it, and answers. A job's panic, which
/// production code never raises, is caught here and answered as an error, so the job's owner
/// never waits on an answer that cannot come.
fn serve(file: &DeviceFile, counters: &Counters, jobs: &Receiver<Work>, answer: &SyncSender<Done>) {
    while let Ok(work) = jobs.recv() {
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| counters.work(file, &work)))
            .unwrap_or_else(|_| Err(unwound()));
        if answer.send(outcome).is_err() {
            return;
        }
    }
}

/// What one worker did: its transfers, their latencies, when it stopped, and the transfers
/// in flight it saw at its completions, summed, and the most.
struct Worked {
    ops: u64,
    latency: Histogram,
    ended: Instant,
    in_flight: u64,
    most_in_flight: usize,
}

impl Counters {
    fn work(&self, file: &DeviceFile, work: &Work) -> Result<Worked, DiskError> {
        let job = &work.job;
        let mut rng = SplitMix64::new(work.seed);
        let mut buf = AlignedBuf::zeroed(job.block, file.alignment())?;
        if job.pattern.writes() {
            rng.fill(buf.as_mut_capacity());
        }
        buf.set_len(job.block)?;
        let unit = file.alignment().get();
        let mut hist = Histogram::new();
        let (mut ops, mut in_flight, mut most_in_flight) = (0u64, 0u64, 0usize);
        let began = *self
            .began
            .lock()
            .map_err(|_| unwound())?
            .get_or_insert_with(Instant::now);
        let deadline = job.budget.and_then(|budget| began.checked_add(budget));
        loop {
            // Claim an operation before issuing it, so the job never exceeds max_ops.
            if self.issued.fetch_add(1, Ordering::Relaxed) >= job.max_ops
                || deadline.is_some_and(|deadline| Instant::now() >= deadline)
            {
                break;
            }
            if job.pattern.writes() {
                // A fresh random word at the head of each of the device's blocks: no block
                // this job writes repeats another, and all stay incompressible.
                for block in buf.as_mut_slice().chunks_mut(unit) {
                    let word = rng.next_u64().to_le_bytes();
                    for (dst, src) in block.iter_mut().zip(word) {
                        *dst = src;
                    }
                }
            }
            let slot = if job.pattern.random() {
                rng.below(work.slots)
            } else {
                self.next
                    .fetch_add(1, Ordering::Relaxed)
                    .checked_rem(work.slots)
                    .unwrap_or(0)
            };
            // In range: `slot < slots`, so the offset ends within `base + span`, which `run`
            // checked.
            let offset = slot.saturating_mul(work.block).saturating_add(job.base);
            // The count in flight is a statistic no decision reads: relaxed ordering keeps it
            // exact as a count, which is all a sample needs.
            self.in_flight.fetch_add(1, Ordering::Relaxed);
            let begin = Instant::now();
            let done = if job.pattern.writes() {
                file.write_all_at(buf.as_slice(), offset).and_then(|()| {
                    if job.sync_each {
                        file.sync_data()
                    } else {
                        Ok(())
                    }
                })
            } else {
                file.read_exact_at(buf.as_mut_slice(), offset)
            };
            let nanos = u64::try_from(begin.elapsed().as_nanos()).unwrap_or(u64::MAX);
            // The count before this transfer leaves: the transfers in flight as it completed.
            let now = self.in_flight.fetch_sub(1, Ordering::Relaxed);
            done?;
            hist.record(nanos);
            ops = ops.saturating_add(1);
            in_flight = in_flight.saturating_add(u64::try_from(now).unwrap_or(u64::MAX));
            most_in_flight = most_in_flight.max(now);
        }
        Ok(Worked {
            ops,
            latency: hist,
            ended: Instant::now(),
            in_flight,
            most_in_flight,
        })
    }
}

fn unwound() -> DiskError {
    invalid("a measurement worker stopped without answering")
}

fn invalid(what: &str) -> DiskError {
    DiskError::Io {
        op: "measure",
        path: std::path::PathBuf::new(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, what.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_block::buf::Alignment;
    use hyper_block::file::CachingRequest;

    fn scratch(len: u64) -> (tempfile::TempDir, DeviceFile) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe");
        let file = DeviceFile::open(
            &path,
            true,
            CachingRequest::PreferDirect,
            Alignment::new(4096).unwrap(),
        )
        .unwrap();
        file.preallocate(len).unwrap();
        (dir, file)
    }

    fn job(pattern: Pattern, depth: usize, max_ops: u64) -> Job {
        Job {
            pattern,
            block: 4096,
            depth,
            base: 0,
            span: 1 << 20,
            // Each test waits on the transfers it counts, never on a wall-clock guess.
            budget: None,
            max_ops,
            sync_each: false,
            seed: 7,
        }
    }

    #[test]
    fn a_job_performs_exactly_its_operation_bound() {
        let (_dir, file) = scratch(1 << 20);
        for depth in [1, 3, 8] {
            let r = run(&file, &job(Pattern::SequentialWrite, depth, 100)).unwrap();
            assert_eq!(r.ops, 100);
            assert_eq!(r.bytes, 100 * 4096);
            assert_eq!(r.latency.count(), 100);
            let r = run(&file, &job(Pattern::RandomRead, depth, 64)).unwrap();
            assert_eq!(r.ops, 64);
            assert_eq!(r.depth, depth);
            assert!(r.achieved >= 1.0 && r.achieved <= depth as f64);
            assert!((1..=depth).contains(&r.achieved_max));
        }
    }

    /// One pool serves a ladder of depths: its workers start once, it never holds more than the
    /// deepest step, and each job still performs exactly its operations.
    #[test]
    fn a_pool_is_reused_and_grows_only_as_deep_as_asked() {
        let (_dir, file) = scratch(1 << 20);
        with_pool(&file, 8, |pool| {
            for (depth, held) in [(1, 1), (4, 4), (2, 4), (8, 8), (3, 8)] {
                let r = pool.run(&job(Pattern::RandomRead, depth, 40))?;
                assert_eq!(r.ops, 40);
                assert_eq!(pool.workers(), held);
            }
            Ok(())
        })
        .unwrap();
    }

    /// A depth past the pool's size, or past what the process budget has left, is refused
    /// before any worker starts.
    #[test]
    fn a_depth_past_the_budget_starts_no_thread() {
        let (_dir, file) = scratch(1 << 20);
        let err = with_pool(&file, 4, |pool| {
            let r = pool.run(&job(Pattern::RandomRead, 5, 1));
            assert_eq!(pool.workers(), 0);
            r
        })
        .unwrap_err();
        assert!(matches!(err, DiskError::Threads { asked: 5, .. }));
        let past = threads::ceiling().unwrap() + 1;
        let err = with_pool(&file, past, |pool| {
            let r = pool.run(&job(Pattern::RandomRead, past, 1));
            assert_eq!(pool.workers(), 0);
            r
        })
        .unwrap_err();
        assert!(matches!(err, DiskError::Threads { asked, .. } if asked == past));
    }

    #[test]
    fn sequential_writes_cover_the_span_with_random_payload() {
        let (_dir, file) = scratch(1 << 20);
        run(&file, &job(Pattern::SequentialWrite, 1, 256)).unwrap();
        let mut back = AlignedBuf::zeroed(1 << 20, file.alignment()).unwrap();
        file.read_exact_at(back.as_mut_capacity(), 0).unwrap();
        for block in back.as_mut_capacity().chunks(4096) {
            assert!(block.iter().any(|&b| b != 0), "a block was never written");
        }
    }

    /// No two blocks a job writes are alike, even the same block written again, so a device
    /// that deduplicates stores every one (audit P09).
    #[test]
    fn every_block_written_is_distinct() {
        let (_dir, file) = scratch(1 << 20);
        let mut j = job(Pattern::SequentialWrite, 1, 512);
        j.block = 8192;
        run(&file, &j).unwrap();
        let mut back = AlignedBuf::zeroed(1 << 20, file.alignment()).unwrap();
        file.read_exact_at(back.as_mut_capacity(), 0).unwrap();
        let blocks: std::collections::HashSet<&[u8]> =
            back.as_mut_capacity().chunks(4096).collect();
        assert_eq!(blocks.len(), (1 << 20) / 4096);
    }

    #[test]
    fn invalid_jobs_are_refused() {
        let (_dir, file) = scratch(1 << 20);
        let mut j = job(Pattern::RandomRead, 1, 1);
        j.block = 1000;
        assert!(run(&file, &j).is_err());
        let j = job(Pattern::RandomRead, 0, 1);
        assert!(run(&file, &j).is_err());
        let mut j = job(Pattern::RandomRead, 1, 1);
        j.span = 100;
        assert!(run(&file, &j).is_err());
        let mut j = job(Pattern::RandomRead, 1, 1);
        j.base = 100;
        assert!(run(&file, &j).is_err());
        // Neither a budget nor a count: nothing would end it.
        let j = job(Pattern::RandomRead, 1, u64::MAX);
        assert!(run(&file, &j).is_err());
    }

    /// A job writes only inside `[base, base + span)`.
    #[test]
    fn a_job_stays_within_its_base_and_span() {
        let (_dir, file) = scratch(1 << 20);
        let mut j = job(Pattern::SequentialWrite, 2, 64);
        j.base = 256 << 10;
        j.span = 256 << 10;
        run(&file, &j).unwrap();
        let mut back = AlignedBuf::zeroed(1 << 20, file.alignment()).unwrap();
        file.read_exact_at(back.as_mut_capacity(), 0).unwrap();
        for (i, block) in back.as_mut_capacity().chunks(4096).enumerate() {
            let written = block.iter().any(|&b| b != 0);
            assert_eq!(written, (64..128).contains(&i), "block {i}");
        }
    }

    #[test]
    fn below_stays_in_range() {
        let mut rng = SplitMix64::new(1);
        for bound in [1u64, 2, 3, 1000, u64::MAX] {
            for _ in 0..1000 {
                assert!(rng.below(bound) < bound);
            }
        }
        assert_eq!(rng.below(0), 0);
    }
}
