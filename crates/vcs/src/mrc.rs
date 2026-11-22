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

use std::collections::BTreeMap;

/// Format: the bins a curve keeps: one for each bit length a distance in windows can have, 0 to 64.
const BINS: usize = 65;

/// Format: the bytes each sampled key costs a curve: its entry in the key map (the key, its slot and its size, with
/// the map's own node share taken as one more entry, a B-tree's node holding at most twice its least), and the two
/// slots it may span (the slot's key and the Fenwick tree's sum).
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
  /// The sampling threshold: a key below it is sampled.
  threshold: u64,
  /// Each sampled key's slot and bytes.
  keys: BTreeMap<u64, (usize, u64)>,
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

impl Curve {
  /// A curve keeping at most `capacity` sampled keys (at least one), counting distances in windows of `granule` bytes
  /// (at least one), sampling every key until its capacity binds.
  pub fn new(capacity: usize, granule: u64) -> Curve {
    let capacity = capacity.max(1);
    Curve {
      capacity,
      granule: granule.max(1),
      threshold: u64::MAX,
      keys: BTreeMap::new(),
      slot_keys: Vec::new(),
      sums: Vec::new(),
      next_slot: 0,
      bins: [0.0; BINS],
      cold: 0.0,
      sampled: 0.0,
      expected: 0.0,
    }
  }

  /// The sampling rate now: the fraction of keys sampled.
  pub fn sampling_rate(&self) -> f64 {
    self.threshold as f64 / 2f64.powi(64)
  }

  /// One read of the object whose uniform hash is `key` and whose length is `bytes`.
  pub fn read(&mut self, key: u64, bytes: u64) {
    self.expected += self.sampling_rate();
    if key >= self.threshold {
      return;
    }
    self.sampled += 1.0;
    match self.keys.get(&key).copied() {
      Some((slot, held)) => {
        let after = self.total().saturating_sub(self.prefix(slot));
        self.count(after, held);
        self.add(slot, held, false);
        self.slot_keys.get_mut(slot).map(|entry| entry.take());
      }
      None => self.cold += 1.0,
    }
    self.place(key, bytes);
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
    let rate = self.sampling_rate();
    let scaled = if rate > 0.0 {
      crate::share::whole_units((crate::share::amount_of(after) / rate).ceil()).saturating_add(held)
    } else {
      u64::MAX
    };
    let windows = scaled.div_ceil(self.granule).max(1);
    let bin = usize::try_from(u64::BITS.saturating_sub(windows.leading_zeros()))
      .unwrap_or(BINS.saturating_sub(1));
    if let Some(slot) = self.bins.get_mut(bin) {
      *slot += 1.0;
    }
  }

  /// Puts `key` at the next slot with `bytes`, renumbering the slots first when none is left.
  fn place(&mut self, key: u64, bytes: u64) {
    if self.next_slot >= self.slot_keys.len() {
      self.renumber();
    }
    let slot = self.next_slot;
    if let Some(entry) = self.slot_keys.get_mut(slot) {
      *entry = Some(key);
    }
    self.add(slot, bytes, true);
    self.keys.insert(key, (slot, bytes));
    self.next_slot = slot.saturating_add(1);
  }

  /// Drops the sampled key with the largest hash and lowers the threshold to it, scaling the counts by the rate's fall
  /// (module docs).
  fn shrink(&mut self) {
    let Some((key, (slot, bytes))) = self.keys.pop_last() else {
      return;
    };
    self.add(slot, bytes, false);
    if let Some(entry) = self.slot_keys.get_mut(slot) {
      *entry = None;
    }
    let before = self.sampling_rate();
    self.threshold = key;
    let after = self.sampling_rate();
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
    self.next_slot = 0;
    for key in live {
      let bytes = self.keys.get(&key).map_or(0, |(_, bytes)| *bytes);
      let slot = self.next_slot;
      if let Some(entry) = self.slot_keys.get_mut(slot) {
        *entry = Some(key);
      }
      self.add(slot, bytes, true);
      self.keys.insert(key, (slot, bytes));
      self.next_slot = slot.saturating_add(1);
    }
  }

  /// Adds (or takes away) `bytes` at `slot` in the Fenwick tree.
  fn add(&mut self, slot: usize, bytes: u64, adding: bool) {
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
    match self.sums.len() {
      0 => 0,
      len => self.prefix(len.saturating_sub(1)),
    }
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
