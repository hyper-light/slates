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
//!   saved bytes most outweigh its time at the exchange (§4.11's cost model). Each fragment
//!   samples every level on one object of each kind it holds.
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
