//! A repository's working set as a miss-ratio curve, built by spatially hashed sampling of reuse distances (A-724
//! piece 2; §4.17.5 "Learned demand").
//!
//! **What it is.** §4.17.5 divides a shard's memory among its repositories by marginal utility: each byte goes where it
//! saves the most misses. That needs, for each repository, how many of its reads would miss in a cache of `c` bytes, for
//! every `c`: its miss-ratio curve. An LRU cache of `c` bytes hits a read exactly when the read's *reuse distance*, the
//! bytes of distinct objects read since the same object was last read, plus its own, is at most `c` [A: Mattson,
//! Gecsei, Slutz and Traiger, "Evaluation techniques for storage hierarchies", IBM Systems Journal 1970]. So the curve
//! is the distribution of reuse distances. Computing it exactly costs memory in the repository's whole footprint, which
//! is what this is meant to bound.
//!
//! **SHARDS, fixed size** [A: Waldspurger, Park, Garthwaite and Ahmad, "Efficient MRC Construction with SHARDS", FAST
//! 2015]. A read is sampled when its key's uniform hash is below a threshold `T`, so the sampling rate is `R = T / 2^64`
//! and a key is either always sampled or never. Distances are measured among the sampled keys alone and scaled by
//! `1 / R`. The fixed-size variant keeps at most `capacity` sampled keys: when one more would pass it, the key with the
//! largest hash leaves and `T` falls to that hash, and the counts already kept are scaled by `R_new / R_old`, so the
//! memory is bounded by `capacity` whatever the repository's size. `capacity` is the caller's to derive from the memory
//! the repository's row may take (`keys_within`). The ratio is the sampled reads' own (misses over sampled reads);
//! `Σ R` over every read counts the reads themselves. SHARDS_adj, which adds the reads expected to be sampled and not
//! sampled to the smallest distance, was measured and rejected here: objects are one window or more, so no read's
//! distance is below its own size, and on a skewed trace (30,000 objects of one to five windows, 300,000 reads, 2,000
//! keys) it counted 19% of the reads as hits at caches under 2,048 windows where the exact curve had 0.4% to 41%.
//!
//! **Accuracy is the capacity's** (measured, `mrc::tests`, M-series Mac, 2026-11-21): on that trace, over six hash
//! salts, 8,000 keys (the paper's default) erred 0.008 to 0.049 in mean absolute miss ratio over caches of 1 to 2^16
//! windows, mixed in sign; 2,000 keys erred +0.063 to +0.088, every salt high, and a fixed-rate sample at the same rate
//! the same (+0.076 to +0.096). The bias is the sample's: a distance is estimated from the few sampled objects within it,
//! and symmetric noise over a distance density that falls with distance moves more reads above a cache than below it.
//! So a caller derives a capacity of thousands of keys (`keys_within` the row's memory), which costs memory only as a
//! repository has that many objects.
//!
//! **Distances in O(log capacity).** Each sampled key holds a slot in time order; a Fenwick tree over the slots
//! [A: Fenwick, "A new data structure for cumulative frequency tables", Software: Practice and Experience 1994] holds
//! each live key's bytes at its slot, so a key's distance is one suffix sum. When the slots run out the live keys (at
//! most `capacity + 1`) are renumbered in order into twice their count, at most twice the capacity, so the memory follows
//! the keys a repository has (a toy repository's curve holds its few) and a renumbering's cost is amortized to O(log
//! capacity) a read.
//!
//! **Bins.** Distances are counted in bins of the content plane's window (`granule`, D-6's chunk, which the caller
//! passes): bin `b` holds the distances of `2^(b-1)` to `2^b - 1` windows, bin 0 none, so the curve is exact at every
//! `granule * (2^b - 1)` bytes and read linearly between, in at most 65 bins whatever the repository's size.
//!
//! **Learning.** A curve keeps every read since it was made until its owner ages it ([`Curve::decay`]): the owner
//! multiplies its counts by what its learned cadence keeps, so a repository's past traffic weighs less as it recedes
//! (§4.17.1 "Every tunable is learned online").
//!
//! **What it is not.** It measures; it divides nothing (that is `crate::share` and A-724 piece 2's partition by marginal
//! utility). Keys are the caller's uniform hashes: the first eight bytes of a BLAKE3 content identity are, and nothing
//! here hashes again. Arithmetic on the counts is in `f64`, where nothing panics; every slot and size is checked.

use std::collections::{BinaryHeap, HashMap};
use std::hash::{BuildHasher, Hasher};

/// Format: the bins a curve keeps: one for each bit length a distance in windows can have, 0 to 64.
const BINS: usize = 65;

/// Shape: reciprocal of the 64-bit hash space (`2^-64`), used to compute the SHARDS sampling rate
/// `T / 2^64` without invoking `f64::exp2` on every read.
const INV_U64_SPACE: f64 = 1.0 / ((1_u128 << u64::BITS) as f64);

/// Identity hasher for [`Curve`]'s keys, which are already the first eight bytes of a cryptographic
/// BLAKE3 content identity (module docs: "nothing here hashes again").
#[derive(Default)]
struct UniformKeyHasher(u64);

impl Hasher for UniformKeyHasher {
  fn finish(&self) -> u64 {
    self.0
  }

  fn write(&mut self, bytes: &[u8]) {
    if let Some(head) = bytes.first_chunk::<{ core::mem::size_of::<u64>() }>() {
      self.0 = u64::from_ne_bytes(*head);
    }
  }

  fn write_u64(&mut self, value: u64) {
    self.0 = value;
  }
}

/// Builder for [`UniformKeyHasher`].
#[derive(Clone, Copy, Debug, Default)]
struct UniformKeyBuildHasher;

impl BuildHasher for UniformKeyBuildHasher {
  type Hasher = UniformKeyHasher;

  fn build_hasher(&self) -> UniformKeyHasher {
    UniformKeyHasher::default()
  }
}

/// Sampling rate `threshold / 2^64` and its reciprocal (0 when `threshold == 0`).
fn rate_pair(threshold: u64) -> (f64, f64) {
  let rate = (threshold as f64) * INV_U64_SPACE;
  let inv = if rate > 0.0 { 1.0 / rate } else { 0.0 };
  (rate, inv)
}

/// Format: the bytes each sampled key costs a curve: its entry in the key hash table (key, slot and
/// size, plus load-factor overhead taken as one more entry) and its entry in the max-heap, plus the
/// two slots it may span (the slot's key and the Fenwick tree's sum).
const BYTES_PER_KEY: usize = core::mem::size_of::<u64>()
  .saturating_mul(6)
  .saturating_add(
    core::mem::size_of::<Option<u64>>()
      .saturating_add(core::mem::size_of::<u64>())
      .saturating_mul(2),
  );

/// The sampled keys a curve keeps within `bytes` of memory (module docs): at least one.
pub fn keys_within(bytes: usize) -> usize {
  bytes.checked_div(BYTES_PER_KEY).unwrap_or(0).max(1)
}

/// One repository's miss-ratio curve (module docs).
#[derive(Clone, Debug)]
pub struct Curve {
  /// The most sampled keys kept.
  capacity: usize,
  /// The window distances are counted in, in bytes (at least one).
  granule: u64,
  /// When `granule` is a power of two, its bit shift so bin rounding avoids integer division.
  granule_shift: Option<u32>,
  /// The sampling threshold: a key below it is sampled.
  threshold: u64,
  /// Precomputed sampling rate `threshold / 2^64`, updated only when `threshold` moves.
  rate: f64,
  /// Precomputed reciprocal `1.0 / rate`, updated only when `threshold` moves.
  inv_rate: f64,
  /// Each sampled key's slot and bytes (O(1) hash lookup over uniform BLAKE3 key prefixes).
  keys: HashMap<u64, (usize, u64), UniformKeyBuildHasher>,
  /// Max-heap of live sampled keys so `shrink` evicts the largest key in O(log capacity). Because
  /// `shrink` sets `threshold = key` and keys `>= threshold` are never re-admitted, every live key
  /// is pushed once and popped at most once.
  largest: BinaryHeap<u64>,
  /// Total bytes of every live sampled key (kept in sync with `sums` so reads avoid a Fenwick total walk).
  total_bytes: u64,
  /// Each slot's key, while live.
  slot_keys: Vec<Option<u64>>,
  /// The Fenwick tree over the slots' bytes (one-based in its own arithmetic, stored from zero).
  sums: Vec<u64>,
  /// The next slot to give.
  next_slot: usize,
  /// The sampled reads in each bin, in sampled units.
  bins: [f64; BINS],
  /// The sampled reads of keys never read before (cold misses), in sampled units.
  cold: f64,
  /// The sampled reads, in sampled units.
  sampled: f64,
  /// `Σ R` over every read: the reads counted, in sampled units.
  expected: f64,
}

impl PartialEq for Curve {
  fn eq(&self, other: &Self) -> bool {
    self.capacity == other.capacity
      && self.granule == other.granule
      && self.threshold == other.threshold
      && self.keys == other.keys
      && self.total_bytes == other.total_bytes
      && self.slot_keys == other.slot_keys
      && self.sums == other.sums
      && self.next_slot == other.next_slot
      && self.bins == other.bins
      && self.cold == other.cold
      && self.sampled == other.sampled
      && self.expected == other.expected
  }
}

impl Curve {
  /// A curve keeping at most `capacity` sampled keys (at least one), counting distances in windows of `granule` bytes
  /// (at least one), sampling every key until its capacity binds.
  pub fn new(capacity: usize, granule: u64) -> Curve {
    let capacity = capacity.max(1);
    let granule = granule.max(1);
    let (rate, inv_rate) = rate_pair(u64::MAX);
    Curve {
      capacity,
      granule,
      granule_shift: granule.is_power_of_two().then(|| granule.trailing_zeros()),
      threshold: u64::MAX,
      rate,
      inv_rate,
      keys: HashMap::with_hasher(UniformKeyBuildHasher),
      largest: BinaryHeap::new(),
      total_bytes: 0,
      slot_keys: Vec::new(),
      sums: Vec::new(),
      next_slot: 0,
      bins: [0.0; BINS],
      cold: 0.0,
      sampled: 0.0,
      expected: 0.0,
    }
  }

  /// Bounds the curve to `capacity` sampled keys (at least one) from now, its owner having derived the bound again:
  /// a lower one drops the largest hashes at once, as a read past it would.
  pub fn bound(&mut self, capacity: usize) {
    self.capacity = capacity.max(1);
    while self.keys.len() > self.capacity {
      self.shrink();
    }
  }

  /// The sampling rate now: the fraction of keys sampled.
  pub fn sampling_rate(&self) -> f64 {
    self.rate
  }

  /// One read of the object whose uniform hash is `key` and whose length is `bytes`.
  pub fn read(&mut self, key: u64, bytes: u64) {
    self.expected += self.rate;
    if key >= self.threshold {
      return;
    }
    self.sampled += 1.0;
    if let Some(&(slot, held)) = self.keys.get(&key) {
      // Back-to-back read of the most recently read key: reuse distance after it is zero, and
      // leaving it in place leaves every other key's prefix sum unchanged without advancing slots.
      if slot.saturating_add(1) == self.next_slot && held == bytes {
        self.count(0, held);
        return;
      }
      let after = self.total_bytes.saturating_sub(self.prefix(slot));
      self.count(after, held);
      self.add(slot, held, false);
      if let Some(entry) = self.slot_keys.get_mut(slot) {
        *entry = None;
      }
      self.place_existing(key, bytes);
      return;
    }
    self.cold += 1.0;
    self.place_new(key, bytes);
    while self.keys.len() > self.capacity {
      self.shrink();
    }
  }

  /// The fraction of reads that would miss an LRU cache of `cache_bytes`, in `[0, 1]`: cold reads, and reads whose
  /// distance is past the cache, the bin it falls in read linearly. One with no reads misses nothing.
  pub fn miss_ratio(&self, cache_bytes: u64) -> f64 {
    if self.sampled <= 0.0 {
      return 0.0;
    }
    (self.missed(cache_bytes) / self.sampled).clamp(0.0, 1.0)
  }

  /// The reads that would miss an LRU cache of `cache_bytes`, in the reads' own units (module docs): the curve's count
  /// a marginal-utility partition compares across repositories.
  pub fn misses(&self, cache_bytes: u64) -> f64 {
    self.miss_ratio(cache_bytes) * self.reads()
  }

  /// The sampled reads that would miss an LRU cache of `cache_bytes`, in sampled units: cold reads, and reads whose
  /// scaled distance is past the cache, the bin it falls in read linearly.
  fn missed(&self, cache_bytes: u64) -> f64 {
    let windows = cache_bytes.checked_div(self.granule).unwrap_or(0);
    let mut past = self.cold;
    for (bin, count) in self.bins.iter().enumerate() {
      let (low, high) = bin_windows(bin);
      if windows >= high {
        continue;
      }
      if windows < low {
        past += count;
        continue;
      }
      // Within the bin: the distances past `windows` of the `high - low + 1` it holds, read linearly.
      let span = high.saturating_sub(low) as f64 + 1.0;
      past += count * (high.saturating_sub(windows) as f64 / span);
    }
    past.max(0.0)
  }

  /// The cache sizes, in bytes, where the curve bends: each bin's last distance, `granule * (2^b - 1)`, from the empty
  /// cache to its last bin holding a read; past the last the curve is flat (cold misses alone). Between two the curve is
  /// a line, so a cache size that saves the most misses per byte from any start is one of these
  /// (`crate::partition`).
  pub fn edges(&self) -> impl Iterator<Item = u64> + '_ {
    let last = self
      .bins
      .iter()
      .rposition(|count| *count > 0.0)
      .unwrap_or(0);
    (0..=last).map(|bin| bin_windows(bin).1.saturating_mul(self.granule))
  }

  /// The bytes of distinct objects read, as the sample estimates them: what a cache holding every one would hold.
  pub fn working_set(&self) -> u64 {
    let rate = self.sampling_rate();
    if rate <= 0.0 {
      return 0;
    }
    crate::share::whole_units(crate::share::amount_of(self.total()) / rate)
  }

  /// The reads counted, every one, sampled or not, in the reads' own units (`Σ R` over every read, over `R`).
  pub fn reads(&self) -> f64 {
    let rate = self.sampling_rate();
    if rate <= 0.0 {
      return 0.0;
    }
    self.expected / rate
  }

  /// Ages the curve (module docs, "Learning"): every count kept is multiplied by `keep`, clamped into `[0, 1]`; the
  /// sampled keys and their last reads stay, so a read after it still finds its distance.
  pub fn decay(&mut self, keep: f64) {
    let keep = if keep.is_finite() {
      keep.clamp(0.0, 1.0)
    } else {
      0.0
    };
    self.scale(keep);
  }

  /// Counts a read's distance: `after`, the bytes of the sampled keys read since its key was, each standing for `1 / R`
  /// keys, so scaled by `1 / R`, then its own `held` bytes, which stand for themselves alone (scaled with the rest, a
  /// three-window object at `R = 0.073` was counted 38 windows away from itself, and the curve's misses at 2,000 keys
  /// overstated by 0.07 on average); in windows rounded up, binned by its bit length.
  fn count(&mut self, after: u64, held: u64) {
    let scaled = if after == 0 {
      held
    } else if self.rate > 0.0 {
      crate::share::whole_units((crate::share::amount_of(after) * self.inv_rate).ceil())
        .saturating_add(held)
    } else {
      u64::MAX
    };
    let windows = match self.granule_shift {
      Some(shift) => {
        if scaled == 0 {
          1
        } else {
          ((scaled.saturating_sub(1)) >> shift).saturating_add(1)
        }
      }
      None => scaled.div_ceil(self.granule).max(1),
    };
    let bin = usize::try_from(u64::BITS.saturating_sub(windows.leading_zeros()))
      .unwrap_or(BINS.saturating_sub(1));
    if let Some(slot) = self.bins.get_mut(bin) {
      *slot += 1.0;
    }
  }

  /// Moves already-tracked `key` to the next slot with `bytes`, renumbering first when no slot is left.
  fn place_existing(&mut self, key: u64, bytes: u64) {
    if self.next_slot >= self.slot_keys.len() {
      self.renumber();
    }
    let slot = self.next_slot;
    if let Some(entry) = self.slot_keys.get_mut(slot) {
      *entry = Some(key);
    }
    self.add(slot, bytes, true);
    if let Some(entry) = self.keys.get_mut(&key) {
      *entry = (slot, bytes);
    }
    self.next_slot = slot.saturating_add(1);
  }

  /// Inserts newly sampled `key` at the next slot with `bytes`, renumbering first when no slot is left.
  fn place_new(&mut self, key: u64, bytes: u64) {
    if self.next_slot >= self.slot_keys.len() {
      self.renumber();
    }
    let slot = self.next_slot;
    if let Some(entry) = self.slot_keys.get_mut(slot) {
      *entry = Some(key);
    }
    self.add(slot, bytes, true);
    self.keys.insert(key, (slot, bytes));
    self.largest.push(key);
    self.next_slot = slot.saturating_add(1);
  }

  /// Drops the sampled key with the largest hash and lowers the threshold to it, scaling the counts by the rate's fall
  /// (module docs).
  fn shrink(&mut self) {
    let popped = loop {
      let Some(key) = self.largest.pop() else {
        return;
      };
      if let Some((slot, bytes)) = self.keys.remove(&key) {
        break (key, slot, bytes);
      }
    };
    let (key, slot, bytes) = popped;
    self.add(slot, bytes, false);
    if let Some(entry) = self.slot_keys.get_mut(slot) {
      *entry = None;
    }
    let before = self.rate;
    self.threshold = key;
    let (after, inv) = rate_pair(key);
    self.rate = after;
    self.inv_rate = inv;
    if before > 0.0 {
      self.scale(after / before);
    }
  }

  /// Multiplies every count by `factor`.
  fn scale(&mut self, factor: f64) {
    for bin in &mut self.bins {
      *bin *= factor;
    }
    self.cold *= factor;
    self.sampled *= factor;
    self.expected *= factor;
  }

  /// Gives the live keys slots `0..` in their order and rebuilds the tree over twice their count and one more, at most
  /// twice the capacity (module docs): so the slots grow with the keys a repository actually has, and at least as many
  /// reads follow a renumbering as it moved keys.
  fn renumber(&mut self) {
    let live: Vec<u64> = self.slot_keys.iter().flatten().copied().collect();
    let slots = live
      .len()
      .saturating_add(1)
      .saturating_mul(2)
      .min(self.capacity.saturating_mul(2))
      .max(live.len().saturating_add(1));
    self.slot_keys.clear();
    self.slot_keys.resize(slots, None);
    self.sums.clear();
    self.sums.resize(slots, 0);
    self.total_bytes = 0;
    self.next_slot = 0;
    for key in live {
      let bytes = self.keys.get(&key).map_or(0, |(_, bytes)| *bytes);
      let slot = self.next_slot;
      if let Some(entry) = self.slot_keys.get_mut(slot) {
        *entry = Some(key);
      }
      self.add(slot, bytes, true);
      if let Some(entry) = self.keys.get_mut(&key) {
        *entry = (slot, bytes);
      }
      self.next_slot = slot.saturating_add(1);
    }
  }

  /// Adds (or takes away) `bytes` at `slot` in the Fenwick tree.
  fn add(&mut self, slot: usize, bytes: u64, adding: bool) {
    self.total_bytes = if adding {
      self.total_bytes.saturating_add(bytes)
    } else {
      self.total_bytes.saturating_sub(bytes)
    };
    let mut at = slot.saturating_add(1);
    while let Some(sum) = self.sums.get_mut(at.wrapping_sub(1)) {
      *sum = if adding {
        sum.saturating_add(bytes)
      } else {
        sum.saturating_sub(bytes)
      };
      at = at.saturating_add(at.isolate_lowest_one());
      if at == 0 || at > self.sums.len() {
        break;
      }
    }
  }

  /// The bytes at slots `0..=slot`.
  fn prefix(&self, slot: usize) -> u64 {
    let mut at = slot.saturating_add(1).min(self.sums.len());
    let mut sum = 0u64;
    while at > 0 {
      sum = sum.saturating_add(self.sums.get(at.saturating_sub(1)).copied().unwrap_or(0));
      at &= at.saturating_sub(1);
    }
    sum
  }

  /// The bytes of every live sampled key.
  fn total(&self) -> u64 {
    self.total_bytes
  }
}

/// The windows bin `bin` holds, its least and its most: bin 0 none (`(1, 0)`), bin `b` from `2^(b-1)` to `2^b - 1`.
fn bin_windows(bin: usize) -> (u64, u64) {
  if bin == 0 {
    return (1, 0);
  }
  let shift = u32::try_from(bin.saturating_sub(1)).unwrap_or(u64::BITS.saturating_sub(1));
  let low = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
  let high = low.saturating_sub(1).saturating_add(low);
  (low, high)
}

#[cfg(test)]
#[path = "mrc/tests.rs"]
mod tests;
