//! What the fragments' encoder learns from the deployment's own objects (A-614 part 2; §4.17.2's
//! tunables: the delta window and depth, the zlib level, the base pack's rebuild point). Nothing
//! here is a constant: every input is a time the owner's clock measured, a size the encoder saw,
//! or a choice the deployment's own git clients made.
//!
//! - **The window** ([`Window`]): for each rank a candidate base held in an object's trials, the
//!   bytes it saved beyond the best before it and the time its trial took, summed over every
//!   fragment encoded. An object tries its candidates up to one rank past the deepest whose
//!   savings, at the owner's exchange, paid for its time, so the edge keeps being measured and the
//!   window grows a rank a fragment while further candidates pay. With nothing measured it tries
//!   the nearest candidate alone: trying every one would make a large first fragment (an import)
//!   cost a delta search for every pair of its objects.
//! - **The level** ([`Levels`]): for each kind of object and each level zlib defines, the bytes
//!   in, the bytes out and the time, summed over sampled objects; the level is the one whose
//!   saved bytes most outweigh its time at the exchange (§4.11's cost model). A kind's levels are
//!   sampled once, on the first object of it the repository encodes (its first window), and each
//!   sample's time is the owner's measured rate ([`Rates`]), never a clock's, so the level is one
//!   function of the bytes and the machine's rates (main's ruling, 2026-11-20).
//! - **The depth** ([`Depth`]): the deepest delta chain a push's pack carried, as the
//!   deployment's own git clients chose it (their `pack.depth`), at most git's own limit; before
//!   any push carried a delta, the encoder makes none.
//! - **The rebuild point** ([`Rebuild`]): rent or buy. The rent is the time clones spent on the
//!   fragments after the base pack, as the owner measured each clone; the price is the last
//!   rebuild's measured time. With no base pack, the first clone's cost is the rent and the first
//!   rebuild is due at once, which measures the price. Ski-rental's break-even is within twice an
//!   offline optimum's cost \[A: Karlin, Manasse, Rudolph and Sleator, Algorithmica 3, 1988\].

use std::collections::BTreeMap;

use crate::object_name::Kind;

/// The owner's clock, monotonic nanoseconds: the shard's (§4.3). The store has none of its own.
pub trait Clock {
  /// Now.
  fn now_ns(&mut self) -> u64;
}

/// What saving bytes is worth in time: `ns` nanoseconds of CPU buy `bytes` bytes, the exchange
/// §4.11's value of a byte (memory pressure) over its value of CPU (load) gives, which the owner
/// derives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exchange {
  /// The nanoseconds.
  pub ns: u64,
  /// The bytes they buy.
  pub bytes: u64,
  /// What deflating and searching cost on this machine, measured once ([`Rates`]): the time a level or a delta trial
  /// is charged when the encoder learns, so the bytes it writes are one function of its input (main's ruling,
  /// 2026-11-20).
  pub rates: Rates,
}

/// What the encoder's work costs on the owner's machine, measured once by the owner and fixed for its life
/// ([`Rates::measure`]), logged with its inputs as every derived value is (R3): the nanoseconds a KiB of input takes at
/// each zlib level, and a KiB of a delta search's source and target. Before this the encoder timed each level it
/// sampled and each trial on the shard's clock, so one snapshot's objects deflated to other bytes from run to run and
/// replica to replica (found by A-745 rule 4's held-answer test, 2026-11-20); now a level and a window are chosen from
/// the bytes each saved, measured on the data, and these rates, so one input gives one output, as A-607 chose its
/// deflate engine by measurement once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rates {
  /// Nanoseconds a KiB of input takes to deflate, by level.
  pub deflate_ns_per_kib: [u64; LEVELS_USIZE],
  /// Nanoseconds a KiB of a delta search's source and target takes.
  pub search_ns_per_kib: u64,
}

/// [`LEVELS`] as an array length.
const LEVELS_USIZE: usize = 10;

/// Format: the bytes in a KiB, the rates' unit of input.
const KIB: u64 = 1024;

/// Format: deflate's window, 32 KiB (RFC 1951 §2.3: distances reach 32,768 bytes back). The rates' corpus is two windows,
/// so every match distance the format allows is met.
const DEFLATE_WINDOW: usize = 32 * 1024;

impl Rates {
  /// The nanoseconds `bytes` of input take to deflate at `level`.
  pub fn deflate_ns(&self, level: u32, bytes: u64) -> u64 {
    let per_kib = usize::try_from(level)
      .ok()
      .and_then(|level| self.deflate_ns_per_kib.get(level))
      .copied()
      .unwrap_or(0);
    per_kib.saturating_mul(bytes) / KIB
  }

  /// The nanoseconds a delta search over a `source`-byte base and a `target`-byte object takes.
  pub fn search_ns(&self, source: u64, target: u64) -> u64 {
    self
      .search_ns_per_kib
      .saturating_mul(source.saturating_add(target))
      / KIB
  }

  /// The rates measured on `clock` (the owner's), over a corpus of two deflate windows of text-like bytes made by a
  /// seeded generator: each level deflating the corpus once, and one delta search of the corpus against a copy of it
  /// with a byte changed in every [`KIB`]. The corpus and the copy are what a pushed text file and its next version
  /// are like; the measure is the owner's to make once and keep.
  pub fn measure(clock: &mut dyn Clock) -> Rates {
    let corpus = corpus(DEFLATE_WINDOW.saturating_mul(2));
    let kib = (u64::try_from(corpus.len()).unwrap_or(u64::MAX) / KIB).max(1);
    let mut deflate_ns_per_kib = [0_u64; LEVELS_USIZE];
    for (level, slot) in (0..LEVELS).zip(deflate_ns_per_kib.iter_mut()) {
      let start = clock.now_ns();
      let _ = super::fragment::deflate(&corpus, level);
      *slot = clock
        .now_ns()
        .saturating_sub(start)
        .checked_div(kib)
        .unwrap_or(0);
    }
    let mut changed = corpus.clone();
    let step = usize::try_from(KIB).unwrap_or(1);
    for byte in changed.iter_mut().step_by(step) {
      *byte = byte.wrapping_add(1);
    }
    let start = clock.now_ns();
    let _ = super::deltas::delta_of(&corpus, &changed);
    let search_ns_per_kib = clock
      .now_ns()
      .saturating_sub(start)
      .checked_div(kib.saturating_mul(2))
      .unwrap_or(0);
    Rates {
      deflate_ns_per_kib,
      search_ns_per_kib,
    }
  }
}

/// `len` text-like bytes: words of lowercase letters drawn by a xorshift generator from a fixed seed, separated by
/// spaces and a line break every few words.
fn corpus(len: usize) -> Vec<u8> {
  // Format: xorshift64's shifts (Marsaglia, "Xorshift RNGs", 2003) and a nonzero seed.
  let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
  let mut next = move || {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
  };
  let mut out = Vec::with_capacity(len);
  while out.len() < len {
    let word = next();
    let letters = (word % 8).saturating_add(1);
    for at in 0..letters {
      let letter = u8::try_from((word >> at.saturating_mul(5).saturating_add(8)) % 26).unwrap_or(0);
      out.push(b'a'.saturating_add(letter));
    }
    out.push(if word % 11 == 0 { b'\n' } else { b' ' });
  }
  out.truncate(len);
  out
}

impl Exchange {
  /// Whether saving `saved` bytes pays for `ns` nanoseconds.
  pub fn pays(&self, saved: u64, ns: u64) -> bool {
    let worth = u128::from(saved).saturating_mul(u128::from(self.ns));
    worth >= u128::from(ns).saturating_mul(u128::from(self.bytes))
  }

  /// What saving `saved` bytes at a cost of `ns` is worth, in the exchange's units, to compare.
  fn net(&self, saved: u64, ns: u64) -> i128 {
    let worth = i128::from(saved).saturating_mul(i128::from(self.ns));
    worth.saturating_sub(i128::from(ns).saturating_mul(i128::from(self.bytes)))
  }
}

/// The delta window (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Window {
  /// `ranks[r]`: the bytes saved and the nanoseconds taken at rank `r + 1`.
  ranks: Vec<(u64, u64)>,
}

impl Window {
  /// How many candidates an object with `available` before it tries (module docs).
  pub fn size(&self, exchange: Exchange, available: usize) -> usize {
    if self.ranks.is_empty() {
      return available.min(1);
    }
    let paying = self
      .ranks
      .iter()
      .rposition(|(saved, ns)| exchange.pays(*saved, *ns))
      .map_or(0, |at| at.saturating_add(1));
    paying.saturating_add(1).min(available)
  }

  /// A trial at `rank` (counted from 1) saved `saved` bytes beyond the best before it, taking
  /// `ns`.
  pub fn record(&mut self, rank: usize, saved: u64, ns: u64) {
    let Some(at) = rank.checked_sub(1) else {
      return;
    };
    if self.ranks.len() <= at {
      self.ranks.resize(at.saturating_add(1), (0, 0));
    }
    if let Some(slot) = self.ranks.get_mut(at) {
      *slot = (slot.0.saturating_add(saved), slot.1.saturating_add(ns));
    }
  }
}

/// Format: the levels zlib defines, 0 (stored) to 9 (`zlib.h`).
pub const LEVELS: u32 = 10;

/// One kind's sums at one level: bytes in, bytes out, nanoseconds.
type Sums = (u64, u64, u64);

/// The zlib level for each kind of object (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Levels {
  samples: BTreeMap<(u8, u32), Sums>,
}

impl Levels {
  /// The level for `kind` at the exchange, or `None` while one of its levels is unmeasured.
  pub fn level(&self, kind: Kind, exchange: Exchange) -> Option<u32> {
    let mut best: Option<(i128, u32)> = None;
    for level in 0..LEVELS {
      let (input, output, ns) = *self.samples.get(&(kind.pack_number(), level))?;
      let net = exchange.net(input.saturating_sub(output), ns);
      if best.is_none_or(|(value, _)| net > value) {
        best = Some((net, level));
      }
    }
    best.map(|(_, level)| level)
  }

  /// `kind`'s object of `input` bytes deflated to `output` at `level`, taking `ns`.
  pub fn record(&mut self, kind: Kind, level: u32, (input, output, ns): Sums) {
    let slot = self.samples.entry((kind.pack_number(), level)).or_default();
    *slot = (
      slot.0.saturating_add(input),
      slot.1.saturating_add(output),
      slot.2.saturating_add(ns),
    );
  }
}

/// Format: the deepest delta chain git takes as `pack.depth` (`builtin/pack-objects.c`: "delta
/// chain depth %d is too deep, forcing %d", with 4095).
pub const DEPTH_LIMIT: u64 = 4095;

/// The deepest delta chain the encoder makes (module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Depth {
  deepest: u64,
}

impl Depth {
  /// A push's pack carried a chain `depth` deep.
  pub fn seen(&mut self, depth: u64) {
    self.deepest = self.deepest.max(depth.min(DEPTH_LIMIT));
  }

  /// The deepest chain to make; zero makes no delta.
  pub fn limit(&self) -> u64 {
    self.deepest
  }
}

/// When a green's base pack is made again (module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rebuild {
  rent_ns: u64,
  price_ns: Option<u64>,
}

impl Rebuild {
  /// A clone spent `ns` on the fragments after the base pack (all of them, before one exists).
  pub fn cloned(&mut self, ns: u64) {
    self.rent_ns = self.rent_ns.saturating_add(ns);
  }

  /// A rebuild took `ns`: the price from now on, and the rent starts over.
  pub fn rebuilt(&mut self, ns: u64) {
    self.price_ns = Some(ns);
    self.rent_ns = 0;
  }

  /// Whether a rebuild is due: some rent was paid, and no price is known or the rent reached it.
  pub fn due(&self) -> bool {
    self.rent_ns > 0 && self.price_ns.is_none_or(|price| self.rent_ns >= price)
  }

  /// The rent paid since the last rebuild.
  pub fn rent_ns(&self) -> u64 {
    self.rent_ns
  }
}
