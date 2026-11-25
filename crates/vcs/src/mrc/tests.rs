//! The miss-ratio curve's tests (A-844, A-724 piece 2): exact against Mattson's LRU stack at sampling rate one, then
//! the sampled curve against the exact one over a skewed trace far past its capacity.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::indexing_slicing,
  clippy::cast_precision_loss,
  clippy::arithmetic_side_effects
)]

use super::{Curve, keys_within};
use proptest::prelude::*;

/// Shape: the window the tests count distances in, in bytes.
const GRANULE: u64 = 4096;

/// A key's uniform hash, as a caller takes it from a BLAKE3 identity.
fn key_of(object: u64) -> u64 {
  let digest = blake3::hash(&object.to_le_bytes());
  u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap())
}

/// Mattson's stack over `reads` (object, bytes), the misses an LRU cache of `windows * GRANULE` bytes takes: a read
/// misses when its object was never read, or the bytes of distinct objects read since, plus its own, are more windows
/// than the cache holds.
fn exact_misses(reads: &[(u64, u64)], windows: u64) -> u64 {
  let mut stack: Vec<(u64, u64)> = Vec::new();
  let mut misses = 0;
  for &(object, bytes) in reads {
    match stack.iter().position(|(held, _)| *held == object) {
      Some(at) => {
        let distance: u64 = stack[at..].iter().map(|(_, b)| *b).sum();
        if distance.div_ceil(GRANULE) > windows {
          misses += 1;
        }
        stack.remove(at);
      }
      None => misses += 1,
    }
    stack.push((object, bytes));
  }
  misses
}

/// Each object's bytes: fixed for its life, as a content's are, one to five windows and a part.
fn bytes_of(object: u64) -> u64 {
  (object % 5) * GRANULE + 1 + (object * 977) % GRANULE
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(200)))]

  /// A-844. Do: read a generated trace of up to 400 reads over up to 40 objects into a curve whose capacity holds
  /// every object (sampling rate one). Expect: at every cache of `2^b - 1` windows, `b` from 0 to 12, the curve's
  /// misses equal Mattson's stack's exactly, and its reads the trace's length.
  #[test]
  fn at_rate_one_the_curve_is_mattsons_stack(
    objects in prop::collection::vec(0u64..40, 1..400),
  ) {
    let reads: Vec<(u64, u64)> = objects.iter().map(|o| (*o, bytes_of(*o))).collect();
    let mut curve = Curve::new(64, GRANULE);
    for &(object, bytes) in &reads {
      curve.read(key_of(object), bytes);
    }
    prop_assert!((curve.reads() - reads.len() as f64).abs() < 1e-6);
    for bits in 0..=12u32 {
      let windows = (1u64 << bits) - 1;
      let exact = exact_misses(&reads, windows) as f64;
      let measured = curve.misses(windows * GRANULE);
      prop_assert!((measured - exact).abs() < 1e-6, "windows {windows}: {measured} vs {exact}");
    }
  }
}

/// A deterministic Zipf-like trace: `reads` reads over `objects` objects, object `i` drawn with weight `1 / (i+1)^0.9`
/// by inverse sampling from a xorshift stream.
fn zipf_trace(objects: u64, reads: usize) -> Vec<(u64, u64)> {
  let weights: Vec<f64> = (0..objects)
    .map(|i| 1.0 / ((i + 1) as f64).powf(0.9))
    .collect();
  let total: f64 = weights.iter().sum();
  let mut cumulative = Vec::with_capacity(weights.len());
  let mut running = 0.0;
  for weight in &weights {
    running += weight / total;
    cumulative.push(running);
  }
  let mut state = 0x9E37_79B9_7F4A_7C15u64;
  (0..reads)
    .map(|_| {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      let draw = (state >> 11) as f64 / (1u64 << 53) as f64;
      let object = cumulative.partition_point(|c| *c < draw) as u64;
      (object, bytes_of(object))
    })
    .collect()
}

/// A-844 (SHARDS, fixed size). Do: read a 300,000-read skewed trace over 30,000 objects of one to five windows into an
/// exact curve (capacity past every object) and, under six hash salts, into sampled curves of 8,000 keys, the paper's
/// default. Expect: each sampled curve fell to a rate below one in three; its mean absolute miss-ratio error over caches
/// of 1 to 2^16 windows is under 0.06 for every salt and under 0.04 averaged over them (measured 0.008 to 0.049, mean
/// 0.033; module docs); and its working set within a tenth of the exact one's.
#[test]
fn a_sampled_curve_follows_the_exact_one_within_its_capacity() {
  let reads = zipf_trace(30_000, 300_000);
  let mut exact = Curve::new(40_000, GRANULE);
  for &(object, bytes) in &reads {
    exact.read(key_of(object), bytes);
  }
  let caches: Vec<u64> = (0..=16).map(|bits| (1u64 << bits) * GRANULE).collect();
  let mut errors = Vec::new();
  for salt in 0..6u64 {
    let mut sampled = Curve::new(8_000, GRANULE);
    for &(object, bytes) in &reads {
      sampled.read(key_of(object ^ (salt << 40)), bytes);
    }
    assert!(
      sampled.sampling_rate() < 1.0 / 3.0,
      "rate {}",
      sampled.sampling_rate()
    );
    let error: f64 = caches
      .iter()
      .map(|cache| (exact.miss_ratio(*cache) - sampled.miss_ratio(*cache)).abs())
      .sum::<f64>()
      / caches.len() as f64;
    assert!(error < 0.06, "salt {salt}: mean absolute error {error}");
    errors.push(error);
    let (whole, estimate) = (exact.working_set() as f64, sampled.working_set() as f64);
    assert!(
      (estimate - whole).abs() < whole / 10.0,
      "salt {salt}: working set {estimate} vs {whole}"
    );
  }
  let mean = errors.iter().sum::<f64>() / errors.len() as f64;
  assert!(mean < 0.04, "mean absolute errors {errors:?}");
}

/// A-844 ("Learning"). Do: read a trace, age the curve by half, read the same trace again. Expect: aging halves the
/// misses at every cache and keeps the ratio; the second pass, finding every object sampled before, adds no cold miss,
/// so the ratio at a cache holding everything falls.
#[test]
fn aging_scales_the_counts_and_keeps_the_last_reads() {
  let reads = zipf_trace(500, 5_000);
  let mut curve = Curve::new(1_000, GRANULE);
  for &(object, bytes) in &reads {
    curve.read(key_of(object), bytes);
  }
  let everything = 1u64 << 40;
  let (misses, ratio) = (curve.misses(GRANULE * 64), curve.miss_ratio(GRANULE * 64));
  let cold = curve.miss_ratio(everything);
  curve.decay(0.5);
  assert!((curve.misses(GRANULE * 64) - misses / 2.0).abs() < 1e-6);
  assert!((curve.miss_ratio(GRANULE * 64) - ratio).abs() < 1e-9);
  for &(object, bytes) in &reads {
    curve.read(key_of(object), bytes);
  }
  assert!(curve.miss_ratio(everything) < cold);
}

/// A-844. Do: ask how many keys fit in a kibibyte, a mebibyte and nothing. Expect: more in more memory, at least one.
#[test]
fn the_keys_kept_follow_the_memory_given() {
  assert_eq!(keys_within(0), 1);
  assert!(keys_within(1 << 20) > keys_within(1 << 10));
  assert!(keys_within(1 << 10) >= 1);
}

/// A walk that re-reads what it read last: `runs` runs over `objects` objects drawn from a xorshift stream, each run
/// one object read one to eight times in a row, as an index walk re-reads a table's windows. Each run's object differs
/// from the one before it.
fn rereading_trace(objects: u64, runs: usize) -> (Vec<(u64, u64)>, u64) {
  let mut state = 0x2545_F491_4F6C_DD1Du64;
  let mut next = || {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
  };
  let (mut reads, mut last, mut changes) = (Vec::new(), None, 0u64);
  while reads.len() < runs.saturating_mul(4) && changes < runs as u64 {
    let object = next() % objects;
    if last == Some(object) {
      continue;
    }
    last = Some(object);
    changes += 1;
    for _ in 0..=(next() % 8) {
      reads.push((object, bytes_of(object)));
    }
  }
  (reads, changes)
}

/// Main 01a20182's item of 2026-11-24 (w-git's guard): the curve cost about a fifth of a git clone's object-choosing
/// walk (`Curve::read` 2,906 and its key map's insert 1,749 of 23,095 work samples), an index walk re-reading a few
/// table windows so the curve never filled and every read went through the map. Do: read a 2,000-run re-reading trace
/// ([`rereading_trace`]) over 300 objects into a curve through `read`, and the same trace into one through `read_moving`
/// (every sampled read moving its key, as before the guard), each at a capacity of 64 keys so its threshold falls and
/// it shrinks, and again at 4,096 (every key sampled). Expect: the key map's inserts at most one a run (a re-read of
/// the key read last inserts nothing); and every answer the reference's: the miss ratio and the misses at every cache
/// of `2^b - 1` windows (`b` 0 to 16) equal to the last bit, the reads, the working set and the edges. Failing first:
/// one insert every sampled read.
#[test]
fn a_reread_of_the_key_read_last_inserts_nothing_and_answers_as_before() {
  let (reads, runs) = rereading_trace(300, 2_000);
  assert!(reads.len() as u64 > runs * 3, "re-reads in the trace");
  for capacity in [64, 4_096] {
    let mut guarded = Curve::new(capacity, GRANULE);
    let mut moving = Curve::new(capacity, GRANULE);
    for &(object, bytes) in &reads {
      guarded.read(key_of(object), bytes);
      moving.read_moving(key_of(object), bytes);
    }
    assert!(
      guarded.key_inserts() <= runs,
      "capacity {capacity}: {} inserts for {runs} runs ({} reads)",
      guarded.key_inserts(),
      reads.len()
    );
    assert!(
      moving.key_inserts() > runs,
      "the reference moves every read"
    );
    assert_same_answers(&guarded, &moving, capacity);
  }
}

/// Holds `guarded`'s answers to `moving`'s, to the last bit.
fn assert_same_answers(guarded: &Curve, moving: &Curve, capacity: usize) {
  for bits in 0..=16u32 {
    let cache = ((1u64 << bits) - 1) * GRANULE;
    assert_eq!(
      guarded.miss_ratio(cache).to_bits(),
      moving.miss_ratio(cache).to_bits(),
      "capacity {capacity}, cache {cache}"
    );
    assert_eq!(
      guarded.misses(cache).to_bits(),
      moving.misses(cache).to_bits(),
      "capacity {capacity}, cache {cache}"
    );
  }
  assert_eq!(guarded.reads().to_bits(), moving.reads().to_bits());
  assert_eq!(guarded.working_set(), moving.working_set());
  assert_eq!(
    guarded.edges().collect::<Vec<_>>(),
    moving.edges().collect::<Vec<_>>()
  );
}
