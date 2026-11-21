//! Measuring what a device actually does, through the path mantle will use.
//!
//! The OS's description of a device is a claim (a virtio disk on flash reports itself
//! rotational; docs/research notes and the Linux probe test show it). Calibration writes a
//! scratch file under the data directory and measures random small reads and sequential large
//! transfers across queue depths, the latency of a durable write, and whether a durable write
//! into space the file system has allocated but never written costs more than one over written
//! space, with the same direct-I/O file layer the store uses. Runs on a loaded machine vary by 2-4x (docs/measurements), so
//! every point is measured in rounds until the 95% confidence interval of its throughput is
//! within the plan's precision of the mean, or the plan's rounds run out, as Georges, Buytaert
//! and Eeckhout's JavaStats stops (OOPSLA 2007, §3.3; docs/research/11 §13). Latency quantiles
//! come from every round's transfers together, and each is reported only when there are
//! enough transfers to estimate it.
//!
//! The scratch file's size is bounded by the plan and by a tenth of the free space, and it is
//! removed however calibration ends.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::measure::{self, Job, Pattern, Pool};
use hyper_block::DiskError;
use hyper_block::buf::Alignment;
use hyper_block::file::{Caching, CachingRequest, DeviceFile};
use hyper_block::issuer::UNDESCRIBED_QUEUE_DEPTH;
use hyper_block::scratch::Scratch;
use hyper_block::threads;

/// What to measure and how hard.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Bytes of scratch file the jobs touch.
    pub span: u64,
    /// Rounds per point.
    pub rounds: Rounds,
    /// Wall-clock budget of one job.
    pub step: Duration,
    /// Queue depths for random reads. Past the last, the ladder quadruples its depth while
    /// each step is faster than the one before beyond both intervals, so it reaches the depth
    /// where throughput stops growing, and goes no deeper than `max_read_depth`, the device's
    /// queue or the process's thread budget (docs/design/measurement.md §8).
    pub random_depths: Vec<usize>,
    /// The deepest random-read point: NVMe queues hold at most 65,535 commands, its 16-bit,
    /// zero-based maximum queue entries less the slot that tells a full queue from an empty
    /// one (NVM Express Base Specification, CAP.MQES).
    pub max_read_depth: usize,
    /// The commands the device queues, as the OS reports it (`nr_requests` on Linux,
    /// `IOCommandPoolSize` on macOS); `None` when it cannot say.
    pub device_queue: Option<u32>,
    /// Queue depths for sequential transfers.
    pub sequential_depths: Vec<usize>,
    /// Small transfer: the alignment unit, 4 KiB on flash (Haas and Leis, PVLDB 2023, §2.2).
    pub small: usize,
    /// Large transfer.
    pub large: usize,
    /// Durable small writes measured (each one write plus a full flush).
    pub durable_writes: u64,
    /// Bytes of each durable sequential write: written, then flushed, one at a time. The
    /// chunk store's largest group commit, so the rate is the ceiling for batched writes.
    pub durable_large: usize,
}

/// How many rounds a measured point runs: at least `min`, then until the 95% confidence
/// interval of its throughput is within `precision` of the mean, up to `max`, as Georges,
/// Buytaert and Eeckhout's JavaStats stops (OOPSLA 2007, §3.3; docs/research/11 §13).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rounds {
    pub min: usize,
    pub max: usize,
    pub precision: f64,
}

impl Rounds {
    /// Three rounds are the fewest that estimate a spread; six are enough for their minimum
    /// and maximum to bracket the median 95% of the time (docs/research/11 §13.3). The
    /// precision, ±5%, is half the ±10% that comparisons between benchmark runs resolve (§16).
    pub const STANDARD: Self = Self {
        min: 3,
        max: 6,
        precision: 0.05,
    };

    /// The most rounds a point runs; an interval needs two.
    pub fn limit(&self) -> usize {
        self.max.max(self.min).max(2)
    }

    /// Whether `throughputs`, one a round, are enough: the limit reached, or at least the
    /// minimum with an interval within the precision.
    pub fn enough(&self, throughputs: &[f64]) -> bool {
        throughputs.len() >= self.limit()
            || (throughputs.len() >= self.min.max(2)
                && relative_interval(throughputs) <= self.precision)
    }
}

impl Plan {
    /// About twenty seconds on a steady fast SSD, three rounds of half-second jobs a point, and
    /// up to twice that on a noisy one (`Rounds::STANDARD`).
    pub fn standard(align: Alignment, device_queue: Option<u32>) -> Self {
        let small = align
            .max(Alignment::new(4096).unwrap_or(Alignment::BYTE))
            .get();
        Self {
            span: 256 << 20,
            rounds: Rounds::STANDARD,
            step: Duration::from_millis(500),
            random_depths: vec![1, 4, 16, 64],
            max_read_depth: 65_535,
            device_queue,
            sequential_depths: vec![1, 4],
            small,
            large: 1 << 20,
            durable_writes: 64,
            durable_large: 32 << 20,
        }
    }
}

/// One measured operating point, over the plan's rounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub depth: usize,
    /// Mean throughput over the rounds.
    pub ops_per_sec: f64,
    pub bytes_per_sec: f64,
    /// Half-width of the throughput's 95% confidence interval, a fraction of its mean.
    pub spread: f64,
    pub rounds: usize,
    /// Latency quantiles over every round's transfers, when enough ran to estimate them.
    pub p50_ns: Option<u64>,
    pub p99_ns: Option<u64>,
    /// Transfers in flight, sampled at each completion over every round: their mean. Below
    /// `depth` when the path could not keep the depth asked (research/26 §2.1).
    pub achieved: f64,
}

/// Transfers needed to report a quantile `q`: enough that the order statistic lies within
/// half the tail's rank of `q` with 95% confidence, `n ≥ 1.96²·q(1−q)/δ²` at `δ = (1−q)/2`
/// (the binomial's normal approximation; docs/research/11 §13.3). 16 for the median, 1,522
/// for p99: 64 durable writes support a median and not a p99, which would be their maximum.
const P50_SAMPLES: u64 = 16;
const P99_SAMPLES: u64 = 1522;

/// Student's t for a two-sided 95% interval at 1 to 30 degrees of freedom; past 30 the normal
/// value, 1.96, is within 4%.
const T95: [f64; 30] = [
    12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
    2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
    2.052, 2.048, 2.045, 2.042,
];

#[derive(Debug, Clone)]
pub struct Calibration {
    pub caching: Caching,
    pub small: usize,
    pub large: usize,
    pub random_read: Vec<Point>,
    /// Random-read throughput still grew at the deepest depth the plan or the measuring
    /// backend allows: saturation lies deeper, and `random_read_saturation` is the fastest
    /// depth measured, not where throughput stops growing.
    pub random_read_capped: bool,
    pub sequential_read: Vec<Point>,
    pub sequential_write: Vec<Point>,
    /// A small write followed by the platform's full flush, one at a time.
    pub durable_write: Point,
    /// Measurement workers the calibration started: its deepest step, never more than the
    /// device's queue or the process's thread budget.
    pub workers: usize,
    /// `durable_large` bytes written sequentially, then a full flush, one at a time.
    pub durable_sequential: Point,
    pub durable_large: usize,
    /// Each round's small durable write into space preallocated and never written, past a
    /// stretch never written as a volume's first data write lands, against the same write again
    /// at once over the place it wrote: the natural logarithm of the ratio of their latencies, a
    /// round each.
    pub first_write_ratios: Vec<f64>,
    pub elapsed: Duration,
}

impl Calibration {
    /// The random-read depth of greatest power, throughput over latency. In a closed loop
    /// latency is depth over throughput, so power is `X²/N`, greatest where the device's
    /// parallel units are just kept busy: Kleinrock's optimum, `N* = X_max · R_min`
    /// (ICC 1979, §3; docs/research/11 §13.3). Past it a deeper queue buys less throughput
    /// than it adds waiting.
    pub fn random_read_knee(&self) -> Option<Point> {
        knee(&self.random_read)
    }

    /// The shallowest random-read depth whose throughput interval overlaps the fastest
    /// point's: where throughput stops growing, as far as the rounds can tell (Georges,
    /// Buytaert and Eeckhout, OOPSLA 2007, §3.3). Past it a read added to the device only
    /// waits. Kleinrock's optimum (`random_read_knee`) lies at or before it, trading
    /// throughput for latency.
    pub fn random_read_saturation(&self) -> Option<Point> {
        saturation(&self.random_read)
    }

    /// The shallowest sequential-read depth of `large` transfers whose throughput interval
    /// overlaps the fastest point's: its bytes in flight are where large reads stop gaining
    /// throughput, and past them bytes added to the device only wait (Little's law; Georges,
    /// Buytaert and Eeckhout, OOPSLA 2007, §3.3).
    pub fn sequential_read_saturation(&self) -> Option<Point> {
        saturation(&self.sequential_read)
    }

    /// The bytes past which reading on costs more of the device than a second read would. A
    /// read takes an access and then its bytes at the transfer rate (Gray and Graefe, SIGMOD
    /// Record 26(4), 1997, §2, eq. 6), so the break-even is the access time times the rate.
    /// Both are taken at saturation, where the device serves the most reads: an access is what
    /// a small random read takes of the saturated device, `1/ops`, less its own bytes at the
    /// sequential saturation rate (docs/research/11 §8.4). `None` without both measurements.
    pub fn read_gap(&self) -> Option<u64> {
        let random = self.random_read_saturation()?;
        let rate = self.sequential_read_saturation()?.bytes_per_sec;
        if random.ops_per_sec <= 0.0 || rate <= 0.0 {
            return None;
        }
        let small = u32::try_from(self.small).map_or(f64::MAX, f64::from);
        let access = (1.0 / random.ops_per_sec - small / rate).max(0.0);
        whole(rate * access)
    }

    /// How many times as long as an overwrite a first write took: the geometric mean of the
    /// rounds' ratios, none without a round.
    pub fn first_write_ratio(&self) -> Option<f64> {
        (!self.first_write_ratios.is_empty()).then(|| mean(&self.first_write_ratios).exp())
    }

    /// Whether a durable write into preallocated, never-written space, past a stretch of it, is
    /// slower than the same write over written space: the 95% confidence interval of the mean of
    /// the rounds' log ratios lies wholly above zero. A round's two writes are one paired
    /// observation, the second following the first at once under the same load, so the test is
    /// of their difference for a zero mean (Jain, "Comparing Systems Using Sample Data", slides
    /// 13-15 and 13-39; docs/research/11 §13.3); the logarithm weighs a ratio and its reciprocal
    /// alike. Two unpaired points, one measured after the other, part only while the load holds
    /// still: beside the serve suite's other cells on windows-x86_64, 9 of 54 calibrations missed
    /// penalties of 2.1x to 30x, the overwrites' interval spread past ±80% in eight of them
    /// (docs/measurements/2026-10-24-first-writes-past-unwritten-space.md). Where a file system
    /// journals the conversion of an extent on its first write, as ext4 does, the flush pays for
    /// it (5.7x on ext4; docs/measurements/2026-09-28-flush-cost-by-extent-state.md), and where it
    /// fills the stretch first, as NTFS does to a write past its valid data length, the write pays
    /// for the stretch; either way the chunk store writes a volume once before use
    /// (docs/design/chunk-store.md §2).
    pub fn first_write_penalty(&self) -> bool {
        half_width(&self.first_write_ratios)
            .is_some_and(|half| mean(&self.first_write_ratios) - half > 0.0)
    }
}

/// `x` rounded down, when it is finite, not negative and at most `u64::MAX`: the conversion of
/// seconds to a `Duration` checks exactly that, and keeps the whole seconds exact.
fn whole(x: f64) -> Option<u64> {
    Duration::try_from_secs_f64(x).ok().map(|d| d.as_secs())
}

/// `a`'s throughput interval lies wholly below `b`'s.
fn slower(a: &Point, b: &Point) -> bool {
    a.ops_per_sec * (1.0 + a.spread) < b.ops_per_sec * (1.0 - b.spread)
}

fn saturation(points: &[Point]) -> Option<Point> {
    let fastest = points
        .iter()
        .filter(|p| p.depth > 0)
        .max_by(|a, b| a.ops_per_sec.total_cmp(&b.ops_per_sec))?;
    points
        .iter()
        .filter(|p| p.depth > 0 && !slower(p, fastest))
        .min_by_key(|p| p.depth)
        .copied()
}

/// Whether the last point is faster than the one before beyond both intervals.
fn still_growing(points: &[Point]) -> bool {
    matches!(points, [.., before, last] if slower(before, last))
}

/// The random-read ladder's next depth: four times the last, or `cap` if that is nearer,
/// while throughput still grows; `None` once it stops growing or the cap is measured.
fn next_depth(points: &[Point], cap: usize) -> Option<usize> {
    let last = points.last()?.depth;
    let next = last.saturating_mul(4).min(cap);
    if still_growing(points) && next > last {
        Some(next)
    } else {
        None
    }
}

fn knee(points: &[Point]) -> Option<Point> {
    points
        .iter()
        .filter(|p| p.depth > 0 && p.ops_per_sec > 0.0)
        .max_by(|a, b| power(a).total_cmp(&power(b)))
        .copied()
}

fn power(p: &Point) -> f64 {
    let depth = u32::try_from(p.depth).map_or(f64::MAX, f64::from);
    p.ops_per_sec * p.ops_per_sec / depth
}

/// Measures the device under `dir`. `available` is the free space the caller knows of; the
/// scratch file never exceeds a tenth of it.
pub fn calibrate(
    dir: &Path,
    align: Alignment,
    available: Option<u64>,
    plan: &Plan,
) -> Result<Calibration, DiskError> {
    let started = Instant::now();
    let span = match available {
        Some(free) => plan.span.min(free / 10),
        None => plan.span,
    };
    let span = align.down_u64(span);
    // Each round of first writes takes its share of the span, at least a small write's.
    let small = u64::try_from(plan.small).unwrap_or(u64::MAX);
    let rounds = u64::try_from(plan.rounds.limit())
        .unwrap_or(u64::MAX)
        .max(1);
    let stride = align
        .down_u64(span.checked_div(rounds).unwrap_or(0))
        .max(small);
    let needed = u64::try_from(plan.large.max(plan.small))
        .unwrap_or(u64::MAX)
        .max(stride.saturating_mul(rounds));
    if span < needed {
        return Err(DiskError::Io {
            op: "calibrate",
            path: dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "too little free space for a calibration file",
            ),
        });
    }
    // The deepest the device can be driven: its queue, the plan's bound and the threads the
    // process budget has left, decided before any worker starts (docs/design/node.md §1.2).
    let queue = plan
        .device_queue
        .and_then(|q| usize::try_from(q).ok())
        .filter(|&q| q > 0)
        .unwrap_or(UNDESCRIBED_QUEUE_DEPTH);
    let cap = plan.max_read_depth.min(queue).min(threads::left()?);
    if cap == 0 {
        return Err(DiskError::Threads {
            path: dir.to_path_buf(),
            asked: 1,
            left: 0,
            ceiling: threads::ceiling()?,
        });
    }
    let scratch = Scratch::create(dir, ".mantle-calibrate")?;
    let file = DeviceFile::open(scratch.path(), false, CachingRequest::PreferDirect, align)?;
    file.preallocate(span)?;
    measure::with_pool(&file, cap, |pool| {
        measured(pool, &file, plan, span, stride, cap, started)
    })
}

/// Every measurement of a calibration, on one pool of workers that grows as deep as its
/// deepest step.
fn measured(
    pool: &mut Pool<'_, '_>,
    file: &DeviceFile,
    plan: &Plan,
    span: u64,
    stride: u64,
    cap: usize,
    started: Instant,
) -> Result<Calibration, DiskError> {
    // Before anything is written, first writes as a volume makes them, each past a stretch never
    // written: a volume's first data write lands past the unwritten rest of its log, and a
    // segment it opens past the unwritten tail of the one before (chunk-store.md §2). NTFS fills
    // the stretch with zeros before such a write completes, the write being past the file's
    // valid data length (research/02 §4.7), and ext4 commits the extent's conversion at the
    // flush (docs/measurements/2026-09-28-flush-cost-by-extent-state.md). Each round writes once
    // at the end of its share of the span, then the same place again at once: a pair under one
    // load (`Calibration::first_write_penalty`). Sequential first writes, each at the end of the
    // last, never paid for a stretch: on windows-11-arm they ran within the overwrites' interval
    // while a volume's first data write took seconds
    // (docs/measurements/2026-10-24-first-writes-past-unwritten-space.md).
    let small = u64::try_from(plan.small).unwrap_or(u64::MAX);
    let once = |base| Job {
        pattern: Pattern::SequentialWrite,
        block: plan.small,
        depth: 1,
        base,
        span: small,
        budget: Some(plan.step.saturating_mul(4)),
        max_ops: 1,
        sync_each: true,
        seed: 0,
    };
    let rounds = plan.rounds.limit();
    let mut first_write_ratios = Vec::with_capacity(rounds);
    for round in 0..rounds {
        let at = first_write_at(u64::try_from(round).unwrap_or(u64::MAX), stride, small);
        let first = pool.run(&once(at))?.elapsed.as_secs_f64();
        let again = pool.run(&once(at))?.elapsed.as_secs_f64();
        let ratio = (first / again).ln();
        if ratio.is_finite() {
            first_write_ratios.push(ratio);
        }
    }

    // Write the whole span first, so reads measure the medium rather than holes, and so
    // later writes overwrite written blocks (docs/measurements/2026-09-28).
    let fill = Job {
        pattern: Pattern::SequentialWrite,
        block: plan.large,
        depth: 1,
        base: 0,
        span,
        // Every block must be written, however long the device takes: its count bounds it.
        budget: None,
        max_ops: span
            .checked_div(u64::try_from(plan.large).unwrap_or(u64::MAX))
            .unwrap_or(0),
        sync_each: false,
        seed: 1,
    };
    pool.run(&fill)?;
    file.sync_data()?;

    let job = |pattern, block, depth, seed| Job {
        pattern,
        block,
        depth,
        base: 0,
        span,
        budget: Some(plan.step),
        max_ops: u64::MAX,
        sync_each: false,
        seed,
    };
    let mut random_read = Vec::with_capacity(plan.random_depths.len());
    let planned = |depths: &[usize]| {
        let mut depths: Vec<usize> = depths.iter().map(|&d| d.min(cap)).collect();
        depths.dedup();
        depths
    };
    for depth in planned(&plan.random_depths) {
        random_read.push(point(pool, plan, |round| {
            job(Pattern::RandomRead, plan.small, depth, round)
        })?);
    }
    // Throughput still growing at the last depth: the ladder goes on, four times deeper each
    // step, no deeper than the plan, the device's queue or the thread budget allow (audit S09).
    while let Some(depth) = next_depth(&random_read, cap) {
        random_read.push(point(pool, plan, |round| {
            job(Pattern::RandomRead, plan.small, depth, round)
        })?);
    }
    let random_read_capped =
        still_growing(&random_read) && random_read.last().is_some_and(|p| p.depth >= cap);
    let mut sequential_read = Vec::with_capacity(plan.sequential_depths.len());
    let mut sequential_write = Vec::with_capacity(plan.sequential_depths.len());
    for depth in planned(&plan.sequential_depths) {
        sequential_read.push(point(pool, plan, |round| {
            job(Pattern::SequentialRead, plan.large, depth, round)
        })?);
        sequential_write.push(point(pool, plan, |round| {
            job(Pattern::SequentialWrite, plan.large, depth, round)
        })?);
    }
    let durable_write = point(pool, plan, |round| Job {
        pattern: Pattern::RandomWrite,
        block: plan.small,
        depth: 1,
        base: 0,
        span,
        budget: Some(plan.step.saturating_mul(4)),
        max_ops: plan.durable_writes,
        sync_each: true,
        seed: round,
    })?;
    // No larger than the scratch file, and whole alignment units.
    let durable_large = usize::try_from(span)
        .map_or(plan.durable_large, |s| plan.durable_large.min(s))
        .checked_div(plan.small)
        .unwrap_or(0)
        .saturating_mul(plan.small)
        .max(plan.small);
    let durable_sequential = point(pool, plan, |round| Job {
        pattern: Pattern::SequentialWrite,
        block: durable_large,
        depth: 1,
        base: 0,
        span,
        budget: Some(plan.step.saturating_mul(4)),
        max_ops: u64::MAX,
        sync_each: true,
        seed: round,
    })?;

    Ok(Calibration {
        caching: file.caching(),
        small: plan.small,
        large: plan.large,
        random_read,
        random_read_capped,
        sequential_read,
        sequential_write,
        durable_write,
        durable_sequential,
        durable_large,
        workers: pool.workers(),
        first_write_ratios,
        elapsed: started.elapsed(),
    })
}

/// Where round `round` of first writes writes its one small write: the end of its share of the
/// span, `stride` bytes, past the rest of that share.
fn first_write_at(round: u64, stride: u64, small: u64) -> u64 {
    round
        .saturating_add(1)
        .saturating_mul(stride)
        .saturating_sub(small)
}

/// Runs a job in rounds, as many as the plan's `Rounds` take.
fn point(
    pool: &mut Pool<'_, '_>,
    plan: &Plan,
    job: impl Fn(u64) -> Job,
) -> Result<Point, DiskError> {
    let max = plan.rounds.limit();
    let mut depth = 1;
    let (mut ops, mut bytes) = (Vec::with_capacity(max), Vec::with_capacity(max));
    let mut latency = crate::histogram::Histogram::new();
    let (mut in_flight, mut completions) = (0.0f64, 0u64);
    for round in 0..max {
        let j = job(u64::try_from(round).unwrap_or(0));
        let r = pool.run(&j)?;
        // Counts of bounded rounds are far below 2^53.
        in_flight += r.achieved * r.ops as f64;
        completions = completions.saturating_add(r.ops);
        depth = j.depth;
        ops.push(r.ops_per_sec());
        bytes.push(r.bytes_per_sec());
        latency.merge(&r.latency);
        if plan.rounds.enough(&ops) {
            break;
        }
    }
    let spread = relative_interval(&ops);
    let samples = latency.count();
    Ok(Point {
        depth,
        ops_per_sec: mean(&ops),
        bytes_per_sec: mean(&bytes),
        spread,
        rounds: ops.len(),
        p50_ns: (samples >= P50_SAMPLES).then(|| latency.p50()),
        p99_ns: (samples >= P99_SAMPLES).then(|| latency.p99()),
        achieved: if completions == 0 {
            0.0
        } else {
            in_flight / completions as f64
        },
    })
}

pub fn mean(xs: &[f64]) -> f64 {
    let n = u32::try_from(xs.len()).map_or(f64::MAX, f64::from);
    xs.iter().sum::<f64>() / n
}

/// The half-width of the 95% confidence interval of the mean of `xs` (Student's t), none for
/// fewer than two.
fn half_width(xs: &[f64]) -> Option<f64> {
    let df = xs.len().checked_sub(1).filter(|&d| d > 0)?;
    let m = mean(xs);
    let n = u32::try_from(xs.len()).map_or(f64::MAX, f64::from);
    let variance = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1.0);
    let t = T95.get(df.saturating_sub(1)).copied().unwrap_or(1.96);
    Some(t * (variance / n).sqrt())
}

/// The half-width of the 95% confidence interval of the mean of `xs`, over the mean.
pub fn relative_interval(xs: &[f64]) -> f64 {
    let m = mean(xs);
    match half_width(xs) {
        Some(half) if m > 0.0 => half / m,
        _ => f64::INFINITY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(depth: usize, ops: f64) -> Point {
        Point {
            depth,
            ops_per_sec: ops,
            bytes_per_sec: 0.0,
            spread: 0.0,
            rounds: 3,
            p50_ns: None,
            p99_ns: None,
            achieved: depth as f64,
        }
    }

    /// At 2^17 small reads a second, a 4 KiB read takes 2^-17 s of the device, 2^-20 s of it
    /// moving its bytes at 2^32 B/s: the access is 7·2^-20 s, in which the device reads
    /// 28 KiB. Without a random-read rate there is none.
    #[test]
    fn the_read_gap_is_an_access_at_the_saturated_transfer_rate() {
        // A 1 MiB sequential read's rate in operations tracks its bytes.
        let sequential = |depth, bytes_per_sec: f64| Point {
            bytes_per_sec,
            ..at(depth, bytes_per_sec / f64::from(1u32 << 20))
        };
        let durable = at(1, 0.0);
        let mut c = Calibration {
            caching: Caching::Direct,
            small: 4096,
            large: 1 << 20,
            random_read: vec![at(1, f64::from(1u32 << 15)), at(4, f64::from(1u32 << 17))],
            random_read_capped: false,
            sequential_read: vec![
                sequential(1, 2.0f64.powi(31)),
                sequential(4, 2.0f64.powi(32)),
            ],
            sequential_write: Vec::new(),
            durable_write: durable,
            durable_sequential: durable,
            durable_large: 1 << 20,
            workers: 4,
            first_write_ratios: Vec::new(),
            elapsed: Duration::ZERO,
        };
        assert_eq!(c.read_gap(), Some(28 << 10));
        c.random_read = vec![at(1, 0.0)];
        assert_eq!(c.read_gap(), None);
    }

    /// Power `X²/N` is greatest where more depth stops buying throughput in proportion.
    #[test]
    fn the_knee_is_the_depth_of_greatest_power() {
        let points = [
            at(1, 15_000.0),
            at(4, 60_000.0),
            at(16, 190_000.0),
            at(64, 235_000.0),
        ];
        assert_eq!(knee(&points).unwrap().depth, 16);
        // Throughput that doubles with depth keeps gaining power.
        assert_eq!(
            knee(&[at(1, 100.0), at(2, 200.0), at(4, 400.0)])
                .unwrap()
                .depth,
            4
        );
        assert_eq!(knee(&[]), None);
    }

    /// Throughput stops growing at the shallowest depth whose interval overlaps the fastest
    /// point's; the power knee, this machine's 16, comes before it when throughput still
    /// grows (docs/measurements/2026-09-29-read-depth.md).
    /// The ladder goes four times deeper while throughput grows, stops at the backend's
    /// limit, and says when throughput was still growing there: 64, then 256, and no 1,024
    /// on a backend of 256 threads (audit S09).
    #[test]
    fn the_ladder_stops_at_the_backends_limit_and_says_so() {
        let at = |depth: usize, ops: f64| Point {
            depth,
            ops_per_sec: ops,
            bytes_per_sec: ops * 4096.0,
            spread: 0.01,
            rounds: 3,
            p50_ns: None,
            p99_ns: None,
            achieved: depth as f64,
        };
        let growing = [at(16, 100.0), at(64, 200.0)];
        assert_eq!(next_depth(&growing, 256), Some(256));
        assert_eq!(next_depth(&growing, 100), Some(100));
        let at_cap = [at(64, 200.0), at(256, 400.0)];
        assert_eq!(next_depth(&at_cap, 256), None);
        assert!(still_growing(&at_cap));
        let flat = [at(64, 200.0), at(256, 201.0)];
        assert_eq!(next_depth(&flat, 1024), None);
        assert!(!still_growing(&flat));
        assert_eq!(next_depth(&[at(4, 10.0)], 256), None);
    }

    #[test]
    fn saturation_is_where_throughput_stops_growing() {
        let with = |depth: usize, ops: f64, spread: f64| Point {
            spread,
            ..at(depth, ops)
        };
        let points = [
            with(1, 14_000.0, 0.03),
            with(4, 54_800.0, 0.03),
            with(16, 165_000.0, 0.03),
            with(64, 214_000.0, 0.03),
            with(256, 218_000.0, 0.03),
        ];
        assert_eq!(knee(&points).unwrap().depth, 16);
        assert_eq!(saturation(&points).unwrap().depth, 64);
        // Still growing at the last depth: the last depth is the best found.
        assert_eq!(saturation(&points[..4]).unwrap().depth, 64);
        assert_eq!(saturation(&[]), None);
    }

    /// One point is slower than another only when their intervals part: 5.7x with tight
    /// intervals is, and a difference inside the noise is not.
    #[test]
    fn a_point_is_slower_only_where_the_intervals_part() {
        let with = |ops: f64, spread: f64| Point {
            spread,
            ..at(1, ops)
        };
        assert!(slower(&with(380.0, 0.05), &with(2_170.0, 0.05)));
        assert!(!slower(&with(2_100.0, 0.05), &with(2_170.0, 0.05)));
        assert!(!slower(&with(380.0, 0.9), &with(2_170.0, 0.9)));
        assert!(!slower(&with(2_170.0, 0.05), &with(380.0, 0.05)));
        assert!(!slower(&with(380.0, f64::INFINITY), &with(2_170.0, 0.05)));
    }

    /// A penalty is a paired interval wholly above one: steady 5.7x is one; ratios about one, or
    /// spread wide, or a single round, are none. A load that changes from round to round, shared
    /// by a round's two writes, leaves the penalty measured, where the same writes taken as two
    /// unpaired points do not part.
    #[test]
    fn a_first_write_penalty_is_a_paired_interval_above_one() {
        let with = |ratios: &[f64]| Calibration {
            caching: Caching::Direct,
            small: 4096,
            large: 1 << 20,
            random_read: Vec::new(),
            random_read_capped: false,
            sequential_read: Vec::new(),
            sequential_write: Vec::new(),
            durable_write: at(1, 0.0),
            durable_sequential: at(1, 0.0),
            durable_large: 1 << 20,
            workers: 1,
            first_write_ratios: ratios.iter().map(|r| r.ln()).collect(),
            elapsed: Duration::ZERO,
        };
        assert!(with(&[5.7, 6.1, 5.2, 6.3]).first_write_penalty());
        assert!(!with(&[1.1, 0.9, 1.05, 0.95]).first_write_penalty());
        assert!(!with(&[60.0, 0.5, 40.0, 0.6]).first_write_penalty());
        assert!(!with(&[60.0]).first_write_penalty());
        assert!(!with(&[]).first_write_penalty());
        let steady = with(&[5.7, 6.1, 5.2, 6.3]).first_write_ratio().unwrap();
        assert!((steady - (5.7f64 * 6.1 * 5.2 * 6.3).powf(0.25)).abs() < 1e-9);
        // Each round's load stretches both of its writes alike: a first write 7.5 times an
        // overwrite, the load from one to twelve times.
        let load = [1.0, 8.0, 2.0, 12.0, 1.0, 5.0];
        let first: Vec<f64> = load.iter().map(|l| 0.3 * l).collect();
        let again: Vec<f64> = load.iter().map(|l| 0.04 * l).collect();
        let ratios: Vec<f64> = first.iter().zip(&again).map(|(f, a)| f / a).collect();
        assert!(with(&ratios).first_write_penalty());
        let unpaired = |latencies: &[f64]| {
            let ops: Vec<f64> = latencies.iter().map(|l| 1.0 / l).collect();
            Point {
                ops_per_sec: mean(&ops),
                spread: relative_interval(&ops),
                ..at(1, 0.0)
            }
        };
        assert!(!slower(&unpaired(&first), &unpaired(&again)));
    }

    /// Every round's first write lands past a stretch never written, a share of the span less
    /// the write, and the last ends inside the span.
    #[test]
    fn each_first_write_lands_past_a_stretch_never_written() {
        let (span, small) = (256u64 << 20, 4096u64);
        let rounds = u64::try_from(Rounds::STANDARD.limit()).unwrap();
        let stride = span / rounds / small * small;
        let mut written = 0;
        for round in 0..rounds {
            let at = first_write_at(round, stride, small);
            assert_eq!(at - written, stride - small);
            written = at + small;
        }
        assert!(written <= span);
    }

    /// A point stops at the minimum once its interval is tight, runs on while it is not, and
    /// stops at the limit whatever its spread.
    #[test]
    fn rounds_stop_when_the_interval_is_tight_or_the_limit_is_reached() {
        let r = Rounds::STANDARD;
        assert!(!r.enough(&[100.0, 100.0]));
        assert!(r.enough(&[100.0, 100.0, 100.0]));
        assert!(!r.enough(&[50.0, 100.0, 150.0]));
        assert!(r.enough(&[50.0, 100.0, 150.0, 50.0, 100.0, 150.0]));
        assert_eq!(r.limit(), 6);
        let loose = Rounds {
            min: 1,
            max: 1,
            precision: 0.05,
        };
        assert_eq!(loose.limit(), 2);
        assert!(!loose.enough(&[100.0]));
    }

    #[test]
    fn the_interval_follows_students_t() {
        // Mean 100, sample deviation 10, n = 4: t(3) = 3.182, half-width 3.182·10/2.
        let xs = [90.0, 100.0, 110.0, 100.0];
        let sd = (200.0f64 / 3.0).sqrt();
        let want = 3.182 * sd / 2.0 / 100.0;
        assert!((relative_interval(&xs) - want).abs() < 1e-9);
        assert!(relative_interval(&[5.0]).is_infinite());
        assert_eq!(relative_interval(&[7.0, 7.0, 7.0]), 0.0);
    }

    #[test]
    fn a_small_calibration_runs_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let align = Alignment::new(4096).unwrap();
        let plan = Plan {
            span: 8 << 20,
            rounds: Rounds {
                min: 2,
                max: 3,
                precision: 0.05,
            },
            step: Duration::from_millis(20),
            random_depths: vec![1, 2],
            // The ladder goes no deeper than planned, however the reads scale.
            max_read_depth: 2,
            device_queue: None,
            sequential_depths: vec![1],
            small: 4096,
            large: 1 << 20,
            durable_writes: 4,
            durable_large: 2 << 20,
        };
        let c = calibrate(dir.path(), align, None, &plan).unwrap();
        assert_eq!(c.random_read.len(), 2);
        assert!(c.random_read.iter().all(|p| p.ops_per_sec > 0.0));
        assert!(c.durable_write.ops_per_sec > 0.0);
        // At most twelve durable writes: too few for a p99 or a median.
        assert_eq!(c.durable_write.p99_ns, None);
        assert_eq!(c.durable_write.p50_ns, None);
        assert!((2..=3).contains(&c.durable_write.rounds));
        assert!(c.durable_sequential.bytes_per_sec > 0.0);
        assert_eq!(c.durable_large, 2 << 20);
        assert_eq!(c.first_write_ratios.len(), 3);
        assert!(c.first_write_ratios.iter().all(|r| r.is_finite()));
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(leftovers.is_empty(), "scratch file left behind");
    }

    /// A calibration that fails leaves its directory as it found it: a file already there,
    /// even one named as scratch files once were, is neither written nor removed, and no
    /// scratch file stays behind (audit S06).
    #[test]
    fn a_failed_calibration_leaves_the_directory_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let theirs = dir
            .path()
            .join(format!(".mantle-calibrate-{}", std::process::id()));
        std::fs::write(&theirs, b"customer-data").unwrap();
        let plan = Plan {
            small: 0,
            ..Plan::standard(Alignment::new(4096).unwrap(), None)
        };
        assert!(calibrate(dir.path(), Alignment::new(4096).unwrap(), None, &plan).is_err());
        assert_eq!(std::fs::read(&theirs).unwrap(), b"customer-data");
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(left, vec![theirs]);
    }

    #[test]
    fn too_little_space_is_refused_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let align = Alignment::new(4096).unwrap();
        let plan = Plan::standard(align, None);
        let err = calibrate(dir.path(), align, Some(1 << 20), &plan).unwrap_err();
        assert!(matches!(
            err,
            DiskError::Io {
                op: "calibrate",
                ..
            }
        ));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
