//! The microbenchmarks of §4.1: syscall cost, fault cost per page class, park/unpark latency,
//! core-to-core ring round trip, memcpy throughput by size, hash throughput, codec throughput
//! per candidate level, and the lock capacity the OS grants.
//!
//! Method: lmbench's baselines-and-subtraction for the fault probe (the map/unmap cost is
//! measured on its own and subtracted) [A: McVoy & Staelin, USENIX'96]; the Kalibera-Jones
//! stopping rule from [`crate::bench`] for every probe [A: Kalibera & Jones, ISMM'13]; pinned
//! partner threads for the core matrix where the OS pins (Linux, Windows) and an affinity hint
//! where it does not (macOS, recorded as such).
//!
//! Every probe takes its wall budget from the caller and reports `quick` when it stopped early.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::bench::{Measurement, bytes_per_second, measure, nanos};
use crate::facts::{CoreFacts, Facts, PageFacts};
use crate::stats::{Percentile, Sample, Xorshift, bootstrap_interval, converged};

/// Fault costs per page class, in nanoseconds per page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultCosts {
  /// Nanoseconds per base-page fault when touching an anonymous mapping (map/unmap subtracted).
  pub base_ns: u64,
  /// The measurement behind `base_ns`, per region.
  pub base_region: Measurement,
  /// The map-and-unmap cost of the region alone, the subtracted baseline.
  pub map_unmap_region: Measurement,
  /// Nanoseconds per base page when the OS pre-populates the mapping (Linux `MAP_POPULATE`),
  /// `None` where the OS has no such call.
  pub populated_ns: Option<u64>,
  /// Nanoseconds per base page inside a transparent-huge-page region, `None` where unavailable.
  pub huge_ns: Option<u64>,
  /// How many base pages each region held.
  pub pages_per_region: u64,
}

/// Park/unpark latency: how long a parked thread takes to run again after its peer unparks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeLatency {
  /// Median, in nanoseconds.
  pub p50_ns: u64,
  /// p99, in nanoseconds.
  pub p99_ns: u64,
  /// The bootstrap interval's lower and upper edges around the median.
  pub lower_ns: u64,
  /// The upper edge.
  pub upper_ns: u64,
  /// How many wake-ups were timed.
  pub samples: u32,
  /// True when the budget ended before the interval converged.
  pub quick: bool,
}

/// The ring round trip between two cores: one cache line handed back and forth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorePairRtt {
  /// The first core's id.
  pub a: u32,
  /// The second core's id.
  pub b: u32,
  /// The round trip, in nanoseconds.
  pub rtt: Measurement,
}

/// How threads were placed for the core matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pinning {
  /// The OS pinned each thread to its core.
  Pinned,
  /// The OS took the placement as a hint only (macOS).
  Hint,
  /// Pinning was refused; threads ran wherever the scheduler put them.
  Refused,
}

/// One point on the memcpy curve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemcpyPoint {
  /// The copy size in bytes.
  pub bytes: u64,
  /// Bytes per second at that size.
  pub bytes_per_second: u64,
  /// The per-copy measurement.
  pub per_copy: Measurement,
}

/// Hash throughput.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashThroughput {
  /// The buffer size hashed, in bytes.
  pub bytes: u64,
  /// BLAKE3 bytes per second.
  pub blake3_bytes_per_second: u64,
  /// The per-buffer measurement.
  pub per_buffer: Measurement,
}

/// One codec at one level over the synthetic corpus.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecPoint {
  /// `lz4` or `zstd`.
  pub codec: String,
  /// The level (0 for LZ4, which has one).
  pub level: i32,
  /// Compression throughput in input bytes per second.
  pub compress_bytes_per_second: u64,
  /// Decompression throughput in output bytes per second.
  pub decompress_bytes_per_second: u64,
  /// Compressed size as parts per thousand of the input.
  pub ratio_permille: u64,
  /// The corpus size in bytes.
  pub bytes: u64,
  /// True when either direction's measurement stopped early.
  pub quick: bool,
}

/// What the OS will let this process lock in RAM.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockCapacity {
  /// Bytes the process may lock, as the OS's limit states it (0 when locking is refused).
  pub bytes: u64,
  /// Where the number came from.
  pub source: String,
  /// Whether a small confirming lock succeeded.
  pub confirmed: bool,
}

/// Shape: the fault probe's region in base pages. Enough faults that two map/unmap syscalls are
/// a small part of a sample, small enough that the region stays within any L2 (GAPS §5).
pub const FAULT_REGION_PAGES: u64 = 256;

/// Shape: above this many cores the ring matrix samples pairs with core 0 instead of all pairs
/// (§4.1: "every core pair, or a sampled subset above 32 cores").
pub const FULL_MATRIX_CORE_LIMIT: usize = 32;

/// Shape: the wake probe checks convergence after every batch of this many round trips.
const WAKE_BATCH: usize = 64;

/// Shape: the zstd candidate levels the cost model chooses among: the fast preset, the default,
/// the high preset, and the maximum (§4.11, D-13).
pub const ZSTD_LEVELS: &[i32] = &[1, 3, 9, 19];

/// Measures the cost of a system call that does nothing.
pub fn syscall(budget: Duration) -> Measurement {
  measure(platform::null_syscall, budget)
}

/// Measures fault costs per page class.
pub fn faults(page: &PageFacts, budget: Duration) -> FaultCosts {
  let region = page.base.saturating_mul(FAULT_REGION_PAGES);
  // Shape: the three sub-probes (map/unmap baseline, base pages, populated pages) share the budget.
  let per_probe = budget / 3;
  let map_unmap_region = measure(
    || platform::map_touch_unmap(region, page.base, Touch::None),
    per_probe,
  );
  let base_region = measure(
    || platform::map_touch_unmap(region, page.base, Touch::Every),
    per_probe,
  );
  let base_ns = base_region
    .median_ns()
    .saturating_sub(map_unmap_region.median_ns())
    / FAULT_REGION_PAGES;
  let populated_ns = platform::populated(region, page.base, per_probe)
    .map(|m| m.median_ns().saturating_sub(map_unmap_region.median_ns()) / FAULT_REGION_PAGES);
  let huge_ns = platform::huge(page, per_probe).map(|(m, pages)| m.median_ns() / pages.max(1));
  FaultCosts {
    base_ns,
    base_region,
    map_unmap_region,
    populated_ns,
    huge_ns,
    pages_per_region: FAULT_REGION_PAGES,
  }
}

/// How a region is touched after mapping.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Touch {
  /// Map and unmap only.
  None,
  /// Write one byte in every page.
  Every,
}

/// Measures park/unpark latency between two threads (both directions pooled).
pub fn wake(budget: Duration) -> WakeLatency {
  let stamp = AtomicU64::new(0);
  let turn = AtomicU32::new(0);
  let stop = AtomicBool::new(false);
  let epoch = Instant::now();
  let mut sample = Sample::new(Vec::new());
  let mut rng = Xorshift::new(Xorshift::SEED);
  let mut quick = false;
  let main = std::thread::current();
  std::thread::scope(|scope| {
    let waiter = scope.spawn(|| {
      loop {
        while turn.load(Ordering::Acquire) != 1 {
          if stop.load(Ordering::Acquire) {
            return;
          }
          std::thread::park();
        }
        let woke = nanos(epoch.elapsed());
        let sent = stamp.load(Ordering::Acquire);
        stamp.store(woke.saturating_sub(sent), Ordering::Release);
        turn.store(2, Ordering::Release);
        main.unpark();
      }
    });
    let started = Instant::now();
    loop {
      for _ in 0..WAKE_BATCH {
        stamp.store(nanos(epoch.elapsed()), Ordering::Release);
        turn.store(1, Ordering::Release);
        waiter.thread().unpark();
        while turn.load(Ordering::Acquire) != 2 {
          std::thread::park();
        }
        sample.push(stamp.load(Ordering::Acquire));
        turn.store(0, Ordering::Release);
      }
      if let Some(interval) = bootstrap_interval(&sample, &mut rng)
        && converged(&interval)
      {
        break;
      }
      if started.elapsed() >= budget {
        quick = true;
        break;
      }
    }
    stop.store(true, Ordering::Release);
    waiter.thread().unpark();
  });
  let interval = bootstrap_interval(&sample, &mut rng);
  WakeLatency {
    p50_ns: sample.median().unwrap_or(0),
    p99_ns: sample.percentile(Percentile::P99).unwrap_or(0),
    lower_ns: interval.map_or(0, |i| i.lower),
    upper_ns: interval.map_or(0, |i| i.upper),
    samples: u32::try_from(sample.len()).unwrap_or(u32::MAX),
    quick,
  }
}

/// Measures the ring round trip for every core pair (or the sampled subset above the limit).
pub fn core_matrix(cores: &[CoreFacts], budget: Duration) -> (Vec<CorePairRtt>, Pinning) {
  let pairs = pair_list(cores);
  if pairs.is_empty() {
    return (Vec::new(), Pinning::Refused);
  }
  let per_pair = budget / u32::try_from(pairs.len()).unwrap_or(u32::MAX).max(1);
  let mut pinning = Pinning::Pinned;
  let mut out = Vec::with_capacity(pairs.len());
  for (a, b) in pairs {
    let (rtt, how) = ring_round_trip(a, b, per_pair);
    pinning = weaker(pinning, how);
    out.push(CorePairRtt { a, b, rtt });
  }
  (out, pinning)
}

fn weaker(x: Pinning, y: Pinning) -> Pinning {
  match (x, y) {
    (Pinning::Refused, _) | (_, Pinning::Refused) => Pinning::Refused,
    (Pinning::Hint, _) | (_, Pinning::Hint) => Pinning::Hint,
    _ => Pinning::Pinned,
  }
}

fn pair_list(cores: &[CoreFacts]) -> Vec<(u32, u32)> {
  let ids: Vec<u32> = cores.iter().map(|c| c.id).collect();
  let mut pairs = Vec::new();
  if ids.len() <= FULL_MATRIX_CORE_LIMIT {
    for (i, a) in ids.iter().enumerate() {
      for b in &ids[i + 1..] {
        pairs.push((*a, *b));
      }
    }
  } else if let Some(first) = ids.first() {
    pairs.extend(ids[1..].iter().map(|b| (*first, *b)));
  }
  pairs
}

/// One pair: the caller's thread is placed on `a`, a partner on `b`; they hand a word back and
/// forth; the per-round-trip time is measured with the harness on the caller's side.
fn ring_round_trip(a: u32, b: u32, budget: Duration) -> (Measurement, Pinning) {
  let turn = AtomicU32::new(0);
  let stop = AtomicBool::new(false);
  let mut pinning = platform::pin_current(a);
  let mut result = None;
  let partner_pin = AtomicU32::new(0);
  std::thread::scope(|scope| {
    scope.spawn(|| {
      partner_pin.store(pinning_code(platform::pin_current(b)), Ordering::Release);
      while !stop.load(Ordering::Acquire) {
        if turn.load(Ordering::Acquire) == 1 {
          turn.store(2, Ordering::Release);
        } else {
          std::hint::spin_loop();
        }
      }
    });
    let m = measure(
      || {
        turn.store(1, Ordering::Release);
        while turn.load(Ordering::Acquire) != 2 {
          std::hint::spin_loop();
        }
        turn.store(0, Ordering::Release);
      },
      budget,
    );
    stop.store(true, Ordering::Release);
    pinning = weaker(
      pinning,
      pinning_from_code(partner_pin.load(Ordering::Acquire)),
    );
    result = Some(m);
  });
  (
    result.unwrap_or(Measurement {
      interval: crate::stats::Interval {
        median: 0,
        lower: 0,
        upper: 0,
      },
      p99_ns: 0,
      min_ns: 0,
      samples: 0,
      batch: 0,
      quick: true,
    }),
    pinning,
  )
}

fn pinning_code(p: Pinning) -> u32 {
  match p {
    Pinning::Pinned => 0,
    Pinning::Hint => 1,
    Pinning::Refused => 2,
  }
}

fn pinning_from_code(c: u32) -> Pinning {
  match c {
    0 => Pinning::Pinned,
    1 => Pinning::Hint,
    _ => Pinning::Refused,
  }
}

/// Measures memcpy throughput at each power-of-two size from the cache line up to `cap`.
pub fn memcpy_curve(cache_line: u64, cap: u64, budget: Duration) -> Vec<MemcpyPoint> {
  let mut sizes = Vec::new();
  let mut size = cache_line.max(1).next_power_of_two();
  while size <= cap {
    sizes.push(size);
    size = size.saturating_mul(2);
    if size == 0 {
      break;
    }
  }
  if sizes.is_empty() {
    return Vec::new();
  }
  let per_point = budget / u32::try_from(sizes.len()).unwrap_or(u32::MAX).max(1);
  let largest = usize::try_from(*sizes.last().unwrap_or(&0)).unwrap_or(0);
  let src = filled(largest, Xorshift::SEED);
  let mut dst = vec![0u8; largest];
  sizes
    .into_iter()
    .map(|bytes| {
      let n = usize::try_from(bytes).unwrap_or(0);
      let per_copy = measure(|| dst[..n].copy_from_slice(&src[..n]), per_point);
      std::hint::black_box(&dst);
      MemcpyPoint {
        bytes,
        bytes_per_second: bytes_per_second(bytes, per_copy.median_ns()),
        per_copy,
      }
    })
    .collect()
}

/// Measures BLAKE3 throughput over a buffer of `bytes`.
pub fn hash(bytes: u64, budget: Duration) -> HashThroughput {
  let buf = filled(usize::try_from(bytes).unwrap_or(0), Xorshift::SEED);
  let per_buffer = measure(
    || {
      std::hint::black_box(blake3::hash(&buf));
    },
    budget,
  );
  HashThroughput {
    bytes,
    blake3_bytes_per_second: bytes_per_second(bytes, per_buffer.median_ns()),
    per_buffer,
  }
}

/// Measures LZ4 and zstd at each candidate level over a synthetic corpus of `bytes`: half
/// text-like (words drawn by a seeded generator) and half incompressible, so both ends of the
/// cost model's input space are represented.
pub fn codecs(bytes: u64, budget: Duration) -> Vec<CodecPoint> {
  let corpus = corpus(usize::try_from(bytes).unwrap_or(0));
  let points = 1 + ZSTD_LEVELS.len();
  let per_point = budget / u32::try_from(points).unwrap_or(u32::MAX).max(1);
  let mut out = Vec::with_capacity(points);
  out.push(lz4_point(&corpus, per_point));
  for level in ZSTD_LEVELS {
    out.push(zstd_point(&corpus, *level, per_point));
  }
  out
}

fn lz4_point(corpus: &[u8], budget: Duration) -> CodecPoint {
  let compressed = lz4_flex::block::compress_prepend_size(corpus);
  let compress = measure(
    || {
      std::hint::black_box(lz4_flex::block::compress_prepend_size(corpus));
    },
    budget / 2,
  );
  let decompress = measure(
    || {
      std::hint::black_box(
        lz4_flex::block::decompress_size_prepended(&compressed).unwrap_or_default(),
      );
    },
    budget / 2,
  );
  point("lz4", 0, corpus, &compressed, compress, decompress)
}

fn zstd_point(corpus: &[u8], level: i32, budget: Duration) -> CodecPoint {
  let compressed = zstd::bulk::compress(corpus, level).unwrap_or_default();
  let compress = measure(
    || {
      std::hint::black_box(zstd::bulk::compress(corpus, level).unwrap_or_default());
    },
    budget / 2,
  );
  let capacity = corpus.len();
  let decompress = measure(
    || {
      std::hint::black_box(zstd::bulk::decompress(&compressed, capacity).unwrap_or_default());
    },
    budget / 2,
  );
  point("zstd", level, corpus, &compressed, compress, decompress)
}

fn point(
  codec: &str,
  level: i32,
  corpus: &[u8],
  compressed: &[u8],
  c: Measurement,
  d: Measurement,
) -> CodecPoint {
  let bytes = u64::try_from(corpus.len()).unwrap_or(u64::MAX);
  /// Format: parts per thousand.
  const PERMILLE: u128 = 1000;
  let ratio = if corpus.is_empty() {
    0
  } else {
    u64::try_from(
      u128::from(u64::try_from(compressed.len()).unwrap_or(0)) * PERMILLE / u128::from(bytes),
    )
    .unwrap_or(u64::MAX)
  };
  CodecPoint {
    codec: codec.to_owned(),
    level,
    compress_bytes_per_second: bytes_per_second(bytes, c.median_ns()),
    decompress_bytes_per_second: bytes_per_second(bytes, d.median_ns()),
    ratio_permille: ratio,
    bytes,
    quick: c.quick || d.quick,
  }
}

/// A buffer of pseudo-random bytes from a seeded generator.
pub fn filled(len: usize, seed: u64) -> Vec<u8> {
  let mut rng = Xorshift::new(seed);
  let mut out = Vec::with_capacity(len);
  while out.len() < len {
    let word = rng.next_u64().to_le_bytes();
    let take = (len - out.len()).min(word.len());
    out.extend_from_slice(&word[..take]);
  }
  out
}

/// The synthetic corpus: the first half text-like, the second half incompressible.
pub fn corpus(len: usize) -> Vec<u8> {
  /// Shape: a small vocabulary of source-like tokens; what matters is repetition with
  /// structure, which is what real code and text have and random bytes do not.
  const WORDS: &[&str] = &[
    "fn ",
    "let ",
    "mut ",
    "self",
    "return ",
    "match ",
    "struct ",
    "impl ",
    "pub ",
    "async ",
    "await",
    "Result<",
    "Option<",
    "Vec<u8>",
    "&mut ",
    "->",
    "{\n",
    "}\n",
    "  ",
    "// ",
    "use std::",
    "::new(",
    ");\n",
    "if ",
    "else ",
    "for ",
    "in ",
    "while ",
    "loop ",
    "break;",
    "true",
    "false",
    "0",
    "1",
    "buffer",
    "offset",
    "length",
    "index",
  ];
  let half = len / 2;
  let mut rng = Xorshift::new(Xorshift::SEED);
  let mut out = Vec::with_capacity(len);
  while out.len() < half {
    let word = WORDS[rng.below(WORDS.len())].as_bytes();
    let take = (half - out.len()).min(word.len());
    out.extend_from_slice(&word[..take]);
  }
  out.extend_from_slice(&filled(len - out.len(), Xorshift::SEED ^ u64::MAX));
  out
}

/// Pins the calling thread to `core` where the OS pins (Linux, Windows), hints where it only
/// hints (macOS), and reports which.
pub fn pin_current_thread(core: u32) -> Pinning {
  platform::pin_current(core)
}

/// Queries the lock capacity: the OS's stated limit, confirmed with one small lock.
pub fn lock_capacity(facts: &Facts) -> LockCapacity {
  platform::lock_capacity(facts)
}

#[cfg(unix)]
mod platform {
  use super::{LockCapacity, Pinning, Touch};
  use crate::bench::Measurement;
  use crate::facts::{Facts, PageFacts};
  use std::ffi::c_void;
  use std::time::Duration;

  pub(super) fn null_syscall() {
    // SAFETY: getppid has no preconditions and cannot fail.
    std::hint::black_box(unsafe { libc::getppid() });
  }

  fn map(len: usize, extra_flags: libc::c_int) -> Option<*mut u8> {
    // SAFETY: an anonymous private mapping with no address hint; the result is checked.
    let p = unsafe {
      libc::mmap(
        std::ptr::null_mut(),
        len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | extra_flags,
        -1,
        0,
      )
    };
    if p == libc::MAP_FAILED {
      None
    } else {
      Some(p.cast::<u8>())
    }
  }

  fn unmap(p: *mut u8, len: usize) {
    // SAFETY: `p` came from `map` with the same `len`.
    unsafe { libc::munmap(p.cast::<c_void>(), len) };
  }

  fn touch_every(p: *mut u8, len: usize, page: usize) {
    let mut at = 0;
    while at < len {
      // SAFETY: `at < len` and the mapping is writable.
      unsafe { p.add(at).write_volatile(1) };
      at += page.max(1);
    }
  }

  pub(super) fn map_touch_unmap(region: u64, page: u64, touch: Touch) {
    let len = usize::try_from(region).unwrap_or(0);
    let Some(p) = map(len, 0) else { return };
    if touch == Touch::Every {
      touch_every(p, len, usize::try_from(page).unwrap_or(1));
    }
    unmap(p, len);
  }

  #[cfg(target_os = "linux")]
  pub(super) fn populated(region: u64, page: u64, budget: Duration) -> Option<Measurement> {
    let len = usize::try_from(region).unwrap_or(0);
    let page = usize::try_from(page).unwrap_or(1);
    map(len, libc::MAP_POPULATE).map(|p| unmap(p, len))?;
    Some(crate::bench::measure(
      || {
        if let Some(p) = map(len, libc::MAP_POPULATE) {
          touch_every(p, len, page);
          unmap(p, len);
        }
      },
      budget,
    ))
  }

  #[cfg(not(target_os = "linux"))]
  pub(super) fn populated(_region: u64, _page: u64, _budget: Duration) -> Option<Measurement> {
    None
  }

  #[cfg(target_os = "linux")]
  pub(super) fn huge(page: &PageFacts, budget: Duration) -> Option<(Measurement, u64)> {
    if !page.transparent_huge {
      return None;
    }
    // Shape: the smallest transparent huge page on every Linux target we build for is 2 MiB;
    // a region of two of them, over-mapped by one so it can be aligned, is the probe.
    let huge = page.huge.first().copied().unwrap_or(2 * 1024 * 1024);
    let huge_usize = usize::try_from(huge).ok()?;
    let pages = huge / page.base.max(1) * 2;
    // Shape: one huge page more than the two-page span, so the span can be aligned within it.
    let len = huge_usize * 3;
    let base = usize::try_from(page.base).ok()?;
    Some((
      crate::bench::measure(
        || {
          if let Some(p) = map(len, 0) {
            let aligned = (p as usize).next_multiple_of(huge_usize);
            let span = huge_usize * 2;
            // SAFETY: `aligned + span <= p + len` because the mapping is one huge page longer.
            unsafe {
              libc::madvise(
                (aligned as *mut u8).cast::<c_void>(),
                span,
                libc::MADV_HUGEPAGE,
              );
            }
            touch_every(aligned as *mut u8, span, base);
            unmap(p, len);
          }
        },
        budget,
      ),
      pages,
    ))
  }

  #[cfg(not(target_os = "linux"))]
  pub(super) fn huge(_page: &PageFacts, _budget: Duration) -> Option<(Measurement, u64)> {
    None
  }

  #[cfg(target_os = "linux")]
  pub(super) fn pin_current(core: u32) -> Pinning {
    // SAFETY: an all-zero libc::cpu_set_t is a valid, if empty, value for the call below to fill.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let index = usize::try_from(core).unwrap_or(0);
    // SAFETY: CPU_SET on a zeroed set with an index below the set's capacity.
    unsafe {
      if index >= usize::try_from(libc::CPU_SETSIZE).unwrap_or(0) {
        return Pinning::Refused;
      }
      libc::CPU_SET(index, &mut set);
    }
    // SAFETY: a valid cpu_set_t of the stated size for the calling thread.
    let rc =
      unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &raw const set) };
    if rc == 0 {
      Pinning::Pinned
    } else {
      Pinning::Refused
    }
  }

  #[cfg(target_os = "macos")]
  unsafe extern "C" {
    fn thread_policy_set(
      thread: libc::mach_port_t,
      flavor: libc::c_uint,
      policy_info: *mut libc::c_int,
      count: libc::c_uint,
    ) -> libc::c_int;
  }

  #[cfg(target_os = "macos")]
  pub(super) fn pin_current(core: u32) -> Pinning {
    /// Format: THREAD_AFFINITY_POLICY, the affinity-tag hint (mach/thread_policy.h).
    const THREAD_AFFINITY_POLICY: libc::c_uint = 4;
    // A tag of zero means "no affinity"; cores are numbered from zero, so the tag is core + 1.
    let mut tag: libc::c_int = libc::c_int::try_from(core).unwrap_or(0).saturating_add(1);
    // SAFETY: the calling thread's own mach port and a one-integer policy of the stated count.
    let rc = unsafe {
      thread_policy_set(
        libc::pthread_mach_thread_np(libc::pthread_self()),
        THREAD_AFFINITY_POLICY,
        &raw mut tag,
        1,
      )
    };
    if rc == libc::KERN_SUCCESS {
      Pinning::Hint
    } else {
      Pinning::Refused
    }
  }

  #[cfg(not(any(target_os = "linux", target_os = "macos")))]
  pub(super) fn pin_current(_core: u32) -> Pinning {
    Pinning::Refused
  }

  pub(super) fn lock_capacity(facts: &Facts) -> LockCapacity {
    let mut limit = libc::rlimit {
      rlim_cur: 0,
      rlim_max: 0,
    };
    // SAFETY: `limit` is a writable rlimit.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &raw mut limit) };
    let (bytes, source) = if rc != 0 {
      (0, "getrlimit(RLIMIT_MEMLOCK) refused".to_owned())
    } else if limit.rlim_cur == libc::RLIM_INFINITY {
      platform_wire_limit(facts)
    } else {
      (limit.rlim_cur, "RLIMIT_MEMLOCK soft limit".to_owned())
    };
    let confirmed = confirm_lock(facts.page.base);
    LockCapacity {
      bytes,
      source,
      confirmed,
    }
  }

  #[cfg(target_os = "macos")]
  fn platform_wire_limit(facts: &Facts) -> (u64, String) {
    let mut value: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: a NUL-terminated name and a writable u64 of the stated length.
    let rc = unsafe {
      libc::sysctlbyname(
        c"vm.user_wire_limit".as_ptr(),
        (&raw mut value).cast::<c_void>(),
        &raw mut len,
        std::ptr::null_mut(),
        0,
      )
    };
    if rc == 0 && value > 0 {
      (
        value.min(facts.memory.total),
        "vm.user_wire_limit".to_owned(),
      )
    } else {
      (
        facts.memory.available,
        "RLIMIT_MEMLOCK unlimited; recorded available memory".to_owned(),
      )
    }
  }

  #[cfg(not(target_os = "macos"))]
  fn platform_wire_limit(facts: &Facts) -> (u64, String) {
    (
      facts.memory.available,
      "RLIMIT_MEMLOCK unlimited; recorded available memory".to_owned(),
    )
  }

  fn confirm_lock(page: u64) -> bool {
    let len = usize::try_from(page).unwrap_or(0);
    let Some(p) = map(len, 0) else { return false };
    // SAFETY: `p` is a mapping of `len` bytes.
    let ok = unsafe { libc::mlock(p.cast::<c_void>(), len) } == 0;
    if ok {
      // SAFETY: the same mapping, locked above.
      unsafe { libc::munlock(p.cast::<c_void>(), len) };
    }
    unmap(p, len);
    ok
  }
}

#[cfg(windows)]
mod platform {
  use super::{LockCapacity, Pinning, Touch};
  use crate::bench::Measurement;
  use crate::facts::{Facts, PageFacts};
  use std::ffi::c_void;
  use std::time::Duration;
  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
  use windows_sys::Win32::System::Memory::{
    GetProcessWorkingSetSize, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    VirtualFree, VirtualLock, VirtualUnlock,
  };
  use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentThread, SetEvent, SetThreadAffinityMask,
  };

  fn event() -> HANDLE {
    thread_local! {
      static EVENT: HANDLE = {
        // SAFETY: an anonymous auto-reset event; the handle lives for the thread.
        unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) }
      };
    }
    EVENT.with(|h| *h)
  }

  pub(super) fn null_syscall() {
    // SAFETY: a valid event handle; SetEvent enters the kernel every call.
    std::hint::black_box(unsafe { SetEvent(event()) });
  }

  fn alloc(len: usize) -> Option<*mut u8> {
    // SAFETY: a fresh committed read/write region; the result is checked.
    let p = unsafe {
      VirtualAlloc(
        std::ptr::null(),
        len,
        MEM_RESERVE | MEM_COMMIT,
        PAGE_READWRITE,
      )
    };
    if p.is_null() {
      None
    } else {
      Some(p.cast::<u8>())
    }
  }

  fn free(p: *mut u8) {
    // SAFETY: `p` came from VirtualAlloc.
    unsafe { VirtualFree(p.cast::<c_void>(), 0, MEM_RELEASE) };
  }

  pub(super) fn map_touch_unmap(region: u64, page: u64, touch: Touch) {
    let len = usize::try_from(region).unwrap_or(0);
    let Some(p) = alloc(len) else { return };
    if touch == Touch::Every {
      let page = usize::try_from(page).unwrap_or(1).max(1);
      let mut at = 0;
      while at < len {
        // SAFETY: `at < len` within a committed region.
        unsafe { p.add(at).write_volatile(1) };
        at += page;
      }
    }
    free(p);
  }

  pub(super) fn populated(_region: u64, _page: u64, _budget: Duration) -> Option<Measurement> {
    None
  }

  pub(super) fn huge(_page: &PageFacts, _budget: Duration) -> Option<(Measurement, u64)> {
    None
  }

  pub(super) fn pin_current(core: u32) -> Pinning {
    if core >= usize::BITS {
      return Pinning::Refused;
    }
    let mask: usize = 1usize << core;
    // SAFETY: the calling thread's pseudo-handle and a one-bit mask.
    let previous = unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) };
    if previous == 0 {
      Pinning::Refused
    } else {
      Pinning::Pinned
    }
  }

  pub(super) fn lock_capacity(facts: &Facts) -> LockCapacity {
    let mut min: usize = 0;
    let mut max: usize = 0;
    // SAFETY: the current process pseudo-handle and two writable usize outputs.
    let ok = unsafe { GetProcessWorkingSetSize(GetCurrentProcess(), &raw mut min, &raw mut max) };
    let bytes = if ok == 0 {
      0
    } else {
      u64::try_from(max)
        .unwrap_or(u64::MAX)
        .min(facts.memory.total)
    };
    let len = usize::try_from(facts.page.base).unwrap_or(0);
    let confirmed = alloc(len).is_some_and(|p| {
      // SAFETY: `p` is a committed region of `len` bytes.
      let locked = unsafe { VirtualLock(p.cast::<c_void>(), len) } != 0;
      if locked {
        // SAFETY: the same region, locked above.
        unsafe { VirtualUnlock(p.cast::<c_void>(), len) };
      }
      free(p);
      locked
    });
    let _ = CloseHandle;
    LockCapacity {
      bytes,
      source: "process maximum working set".to_owned(),
      confirmed,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::facts::Facts;

  fn short() -> Duration {
    Duration::from_millis(40)
  }

  #[test]
  fn a_syscall_costs_something_and_is_measured_with_an_interval() {
    let m = syscall(short());
    assert!(m.median_ns() > 0, "{m:?}");
    assert!(m.interval.lower <= m.interval.upper);
  }

  #[test]
  fn faults_cost_more_than_map_and_unmap_alone() {
    let facts = Facts::query();
    let f = faults(&facts.page, short());
    assert!(
      f.base_region.median_ns() > f.map_unmap_region.median_ns(),
      "{f:?}"
    );
    assert!(f.base_ns > 0, "{f:?}");
    assert_eq!(f.pages_per_region, FAULT_REGION_PAGES);
  }

  #[test]
  fn a_wake_is_timed_between_two_threads() {
    let w = wake(short());
    assert!(w.samples >= u32::try_from(WAKE_BATCH).unwrap(), "{w:?}");
    assert!(w.p50_ns > 0 && w.p99_ns >= w.p50_ns, "{w:?}");
  }

  #[test]
  fn the_core_matrix_covers_every_pair_below_the_limit() {
    let facts = Facts::query();
    let n = facts.cores.len().min(4);
    let (rtts, pinning) = core_matrix(&facts.cores[..n], short());
    assert_eq!(rtts.len(), n * (n - 1) / 2);
    assert!(rtts.iter().all(|p| p.rtt.median_ns() > 0), "{rtts:?}");
    // Linux and Windows pin; macOS on Apple silicon refuses the affinity hint (KERN_NOT_SUPPORTED),
    // and the profile records exactly that.
    if cfg!(target_os = "macos") {
      assert!(
        matches!(pinning, Pinning::Hint | Pinning::Refused),
        "{pinning:?}"
      );
    } else {
      assert_eq!(pinning, Pinning::Pinned);
    }
  }

  #[test]
  fn above_the_limit_pairs_are_sampled_from_core_zero() {
    let cores: Vec<CoreFacts> = (0..40)
      .map(|id| CoreFacts {
        id,
        class: crate::facts::CoreClass::Unknown,
        level: 0,
        numa: 0,
        l2_bytes: 0,
      })
      .collect();
    let pairs = pair_list(&cores);
    assert_eq!(pairs.len(), 39);
    assert!(pairs.iter().all(|(a, _)| *a == 0));
  }

  #[test]
  fn the_memcpy_curve_is_ascending_in_size_and_every_point_has_throughput() {
    let points = memcpy_curve(64, 64 * 1024, short());
    assert_eq!(points.len(), 11);
    assert!(points.windows(2).all(|w| w[0].bytes < w[1].bytes));
    assert!(points.iter().all(|p| p.bytes_per_second > 0), "{points:?}");
  }

  #[test]
  fn hashing_and_codecs_report_throughput_and_a_ratio_between_the_halves() {
    let h = hash(64 * 1024, short());
    assert!(h.blake3_bytes_per_second > 0, "{h:?}");
    let c = codecs(64 * 1024, Duration::from_millis(200));
    assert_eq!(c.len(), 1 + ZSTD_LEVELS.len());
    for p in &c {
      assert!(
        p.compress_bytes_per_second > 0 && p.decompress_bytes_per_second > 0,
        "{p:?}"
      );
      // Half the corpus is incompressible, so the ratio sits between one half and one.
      assert!(p.ratio_permille > 500 && p.ratio_permille <= 1000, "{p:?}");
    }
  }

  #[test]
  fn the_corpus_halves_are_distinct_and_deterministic() {
    let a = corpus(4096);
    let b = corpus(4096);
    assert_eq!(a, b);
    assert!(a[..2048].iter().all(u8::is_ascii));
    assert!(a[2048..].iter().any(|b| !b.is_ascii()));
  }

  #[test]
  fn lock_capacity_reports_a_source_and_a_confirmation() {
    let facts = Facts::query();
    let l = lock_capacity(&facts);
    assert!(!l.source.is_empty());
    assert!(l.confirmed, "{l:?}");
  }
}
