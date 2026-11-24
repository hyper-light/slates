//! A run's key filter (goal 93; r536, main's ruling of 2026-11-23 19:54). Under load, a push's lookups spent their
//! time resolving a table through the content store (`ContentStore::get` and the shared object's atomics behind it)
//! for every sealed run they walked, and its duplicate checks are nearly all misses. Mercurial's keys are hashes, so
//! every run's fences span nearly the whole key space and a fence never passes a run over. So each run keeps a Bloom
//! filter of its keys (Bloom 1970, "Space/time trade-offs in hash coding with allowable errors", CACM 13(7)): a run
//! whose filter says no is passed over without a resolve, and a filter never says no to a key its run holds.
//!
//! A lookup hashes its key once ([`KeyHash::of`]) and probes every run's filter from that hash by double hashing
//! (Kirsch and Mitzenmacher 2006, "Less hashing, same performance: building a better Bloom filter", ESA), so the cost
//! of a run passed over is its probes, no resolve.
//!
//! **Size.** The rate a filter may wrongly say yes is derived, not chosen ([`KeyFilter::sized`]): a miss's false
//! resolves summed over every run a lookup may walk stay under one half. A filter is sized once, for the rows its run
//! will hold, when the run's first table is written, and is never resized; its bytes are charged with the run's
//! fences. An empty filter ([`KeyFilter::default`]) says yes to every key: a fork's copy of a run it shares carries
//! none (`Index::fork`), so a fork does not copy its parent's filters.

use std::hash::{DefaultHasher, Hasher};
use std::mem::size_of;

use crate::index::TIER_BASE;

/// Shape: the bits of a filter's word.
const WORD_BITS: u64 = u64::BITS as u64;

/// Shape: the bytes of a filter's word, what each word charges the store.
const WORD_BYTES: u64 = size_of::<u64>() as u64;

/// Derived: log₂ e (1.442695…) in thousandths, rounded up, so a filter is never smaller than the formula asks: an
/// optimal filter of `k` probes takes `k · log₂ e` bits a key (Kirsch and Mitzenmacher 2006, §2).
const LOG2_E_THOUSANDTHS: u64 = 1_443;

/// Shape: the scale of [`LOG2_E_THOUSANDTHS`].
const THOUSANDTHS: u64 = 1_000;

/// Derived: the runs a lookup walks are its index's own and as many again that it shares with a parent, each set held
/// within the size-tiered bound (`Index::tiered_due`).
const OWN_AND_SHARED: u64 = 2;

/// Derived: a miss's false resolves over every run it may walk, at most one over this (so under one, main's ruling).
const FALSE_RESOLVES_PER_MISS_INVERSE: u64 = 2;

/// Format: golden-ratio Weyl increment `⌊2^64 / φ⌋` and Stafford Variant 13 SplitMix64 mixing
/// multipliers (Steele, Lea, and Flood 2014, "Fast Splittable Pseudorandom Number Generators",
/// OOPSLA 2014, Fig. 4) used to derive the independent second double-hashing word from one SipHash
/// evaluation.
const SPLITMIX_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
/// Format: first Stafford Variant 13 avalanche multiplier (Steele, Lea, and Flood 2014, Fig. 4).
const SPLITMIX_MUL_1: u64 = 0xBF58_476D_1CE4_E5B9;
/// Format: second Stafford Variant 13 avalanche multiplier (Steele, Lea, and Flood 2014, Fig. 4).
const SPLITMIX_MUL_2: u64 = 0x94D0_49BB_1331_11EB;
/// Format: SplitMix64 first shift width (Steele, Lea, and Flood 2014, Fig. 4).
const SPLITMIX_SHIFT_1: u32 = 30;
/// Format: SplitMix64 second shift width (Steele, Lea, and Flood 2014, Fig. 4).
const SPLITMIX_SHIFT_2: u32 = 27;
/// Format: SplitMix64 final shift width (Steele, Lea, and Flood 2014, Fig. 4).
const SPLITMIX_SHIFT_3: u32 = 31;

/// A key's hash, taken once a lookup and probed into every run's filter ([`KeyFilter::may_hold`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct KeyHash {
  first: u64,
  step: u64,
}

impl KeyHash {
  /// The hash of `key`: one SipHash word and an independent SplitMix64-mixed second word forced
  /// odd for double hashing (Kirsch and Mitzenmacher 2006).
  pub(crate) fn of(key: &[u8]) -> KeyHash {
    let mut hasher = DefaultHasher::new();
    hasher.write(key);
    let first = hasher.finish();
    let mut second = first.wrapping_add(SPLITMIX_GAMMA);
    second = (second ^ (second >> SPLITMIX_SHIFT_1)).wrapping_mul(SPLITMIX_MUL_1);
    second = (second ^ (second >> SPLITMIX_SHIFT_2)).wrapping_mul(SPLITMIX_MUL_2);
    second ^= second >> SPLITMIX_SHIFT_3;
    KeyHash {
      first,
      step: second | 1,
    }
  }

  /// The `probe`-th bit of a filter of `bits` bits, mapped uniformly onto `[0, bits)` via fast
  /// multiply-high range reduction (Lemire 2019, "Fast Random Integer Generation in an Interval",
  /// ACM TOMS 45(1)) to avoid a 64-bit integer division per probe.
  fn bit(&self, probe: u32, bits: u64) -> Option<u64> {
    if bits == 0 {
      return None;
    }
    let mixed = self
      .first
      .wrapping_add(u64::from(probe).wrapping_mul(self.step));
    let wide = u128::from(mixed).wrapping_mul(u128::from(bits));
    u64::try_from(wide >> u64::BITS).ok()
  }
}

/// A run's key filter (module docs): its words and how many bits a key sets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct KeyFilter {
  words: Box<[u64]>,
  probes: u32,
}

impl KeyFilter {
  /// Derived: a filter for `rows` keys in a store of `budget` bytes. Each row takes a byte of the budget at least, so
  /// an index's tables hold at most `budget` bytes and its own runs number at most ⌊log₃ budget⌋ + 1 (the size-tiered
  /// bound, base [`TIER_BASE`], with a seal of one byte at fewest); a lookup walks [`OWN_AND_SHARED`] times that, `R`. A key sets `k = ⌈log₂(2R)⌉`
  /// bits ([`FALSE_RESOLVES_PER_MISS_INVERSE`] = 2), and the filter takes `k · log₂ e` bits a key, rounded up to whole
  /// words, so a miss passes a run with probability `2^-k ≤ 1/(2R)` and resolves under half a table over all `R`.
  /// For the 64 GiB budget of the intake harness: `R` = 46, `k` = 7, 10.1 bits a key. A size past the machine's
  /// words is no filter: it says yes to every key.
  pub(crate) fn sized(rows: u64, budget: u64) -> KeyFilter {
    let own = budget
      .checked_ilog(TIER_BASE)
      .unwrap_or(0)
      .saturating_add(1);
    let walked = u64::from(own).saturating_mul(OWN_AND_SHARED);
    let Some(probes) = walked
      .saturating_mul(FALSE_RESOLVES_PER_MISS_INVERSE)
      .checked_next_power_of_two()
      .map(u64::trailing_zeros)
    else {
      return KeyFilter::default();
    };
    let bits = rows
      .saturating_mul(u64::from(probes))
      .saturating_mul(LOG2_E_THOUSANDTHS)
      .div_ceil(THOUSANDTHS);
    let Ok(words) = usize::try_from(bits.div_ceil(WORD_BITS).max(1)) else {
      return KeyFilter::default();
    };
    KeyFilter {
      words: vec![0; words].into_boxed_slice(),
      probes,
    }
  }

  /// Whether this filter is empty (says yes to every key without probing).
  pub(crate) fn is_empty(&self) -> bool {
    self.words.is_empty() || self.probes == 0
  }

  /// The bytes the filter charges the store.
  pub(crate) fn bytes(&self) -> u64 {
    u64::try_from(self.words.len())
      .unwrap_or(u64::MAX)
      .saturating_mul(WORD_BYTES)
  }

  /// The filter's length in bits.
  fn bits(&self) -> u64 {
    u64::try_from(self.words.len())
      .unwrap_or(u64::MAX)
      .saturating_mul(WORD_BITS)
  }

  /// Notes a key of the run.
  pub(crate) fn add(&mut self, hash: &KeyHash) {
    let bits = self.bits();
    for probe in 0..self.probes {
      let Some((word, mask)) = hash.bit(probe, bits).and_then(word_and_mask) else {
        return;
      };
      if let Some(word) = self.words.get_mut(word) {
        *word |= mask;
      }
    }
  }

  /// Whether the run may hold the key: false only when it does not; true for an empty filter.
  pub(crate) fn may_hold(&self, hash: &KeyHash) -> bool {
    let bits = self.bits();
    (0..self.probes).all(|probe| {
      hash
        .bit(probe, bits)
        .and_then(word_and_mask)
        .is_none_or(|(word, mask)| self.words.get(word).is_none_or(|word| word & mask != 0))
    })
  }
}

/// Bit `bit` of a filter: its word and the mask of it in that word.
fn word_and_mask(bit: u64) -> Option<(usize, u64)> {
  let word = usize::try_from(bit.checked_div(WORD_BITS)?).ok()?;
  let shift = u32::try_from(bit.checked_rem(WORD_BITS)?).ok()?;
  Some((word, 1_u64.checked_shl(shift)?))
}
