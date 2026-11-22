//! A shard's memory divided among its repositories by marginal utility over their miss-ratio curves, above a floor
//! that keeps every repository served (A-724 piece 2; §4.17.5 "Shares").
//!
//! **What it is.** A byte of cache is worth what it saves: the misses a second a repository's working set would no
//! longer take. Each repository's miss-ratio curve (`crate::mrc::Curve`) says how many of its reads miss a cache of
//! any size, and its rate (`crate::rate::Rates`) turns that into misses a second, so the memory one shard can admit
//! (§4.2) goes where it saves the most [A: Qureshi and Patt, "Utility-Based Cache Partitioning", MICRO 2006].
//!
//! **The floor.** First every repository is given the lesser of the room over the repositories served now (A-188's
//! share, its "clients served now" counted as repositories) and what its curve can use (its working set, no further
//! than its last bin edge; nothing for a curve whose every read is a first one): a small repository always fits
//! whatever a monorepo's curve says, and a hot one never takes another's floor (§4.17.5 "Admission and backpressure").
//! A floor is a guarantee, not a grant: given whole whatever the repository needs, `room / n` each would leave the
//! lookahead nothing to divide.
//!
//! **The rest, by lookahead.** A curve need not be convex (a scan's curve stays flat until its whole loop fits, then
//! falls at once), so giving the next byte to the steepest curve can starve the repository whose saving comes only after
//! a large step. Qureshi and Patt's lookahead asks, for each repository, the extension from its allocation now that
//! saves the most misses a second *per byte* over the whole extension, and gives the best one; then asks again. A
//! curve is a line between its bin edges (`Curve::edges`), so the best extension ends at an edge, or at the room left
//! where no edge fits in it. A heap keeps each repository's best; only the one just given, or one whose best no longer
//! fits the room left, is asked again, so a division costs about `n · e · log n` look-ups of `e` edges each (at most
//! 65 a curve). Talus [A: Beckmann and Sanchez, HPCA 2015] makes a curve convex by splitting a repository's room
//! between two shadow caches; with the lookahead's whole-extension view a non-convex curve is never starved here, so
//! that is left for a measurement to justify.
//!
//! **Work-conserving.** Room no repository's curve would use (each flat past its working set, or a rate of zero)
//! is returned as `spare`: the room a work is lent and taken back first (§4.17.5), never given to a repository that
//! saves nothing with it.
//!
//! **What it is not.** It divides; it measures nothing and admits nothing. A repository's misses a second come from
//! its owner: its curve's misses over its horizon in seconds (`Want::per_s`). Arithmetic is in `f64` for the savings
//! and checked in integers for the bytes.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::mrc::Curve;

/// One repository's want (module docs).
#[derive(Clone, Copy, Debug)]
pub struct Want<'a> {
  /// Its working set's miss-ratio curve.
  pub curve: &'a Curve,
  /// What turns the curve's misses into misses a second: one over the horizon its counts were aged at, in seconds. A
  /// value that is not positive and finite wants nothing past the floor.
  pub per_s: f64,
}

/// What the room was divided into (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Division {
  /// Each repository's bytes, in the wants' order: its floor and what its curve won.
  pub given: Vec<u64>,
  /// The room no curve would use, to lend.
  pub spare: u64,
}

/// A repository's best extension from its allocation now: the misses a second it saves per byte, and its bytes.
#[derive(Clone, Copy, Debug)]
struct Best {
  /// Misses a second saved per byte.
  per_byte: f64,
  /// The extension, in bytes (at least one).
  bytes: u64,
  /// The repository.
  at: usize,
}

impl PartialEq for Best {
  fn eq(&self, other: &Self) -> bool {
    self.cmp(other) == Ordering::Equal
  }
}

impl Eq for Best {}

impl PartialOrd for Best {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for Best {
  /// The most saved per byte first; then the smaller extension, then the earlier repository, so a division is
  /// deterministic.
  fn cmp(&self, other: &Self) -> Ordering {
    self
      .per_byte
      .total_cmp(&other.per_byte)
      .then_with(|| other.bytes.cmp(&self.bytes))
      .then_with(|| other.at.cmp(&self.at))
  }
}

/// Divides `room` bytes among `wants` (module docs): the floor to each, then the rest by lookahead.
pub fn partition(room: u64, wants: &[Want<'_>]) -> Division {
  let count = u64::try_from(wants.len()).unwrap_or(u64::MAX);
  let floor = room.checked_div(count).unwrap_or(0);
  let mut given: Vec<u64> = wants
    .iter()
    .map(|want| floor.min(usable(want.curve)))
    .collect();
  let mut left = room.saturating_sub(
    given
      .iter()
      .fold(0u64, |sum, bytes| sum.saturating_add(*bytes)),
  );
  let mut heap: BinaryHeap<Best> = wants
    .iter()
    .zip(&given)
    .enumerate()
    .filter_map(|(at, (want, held))| best(want, at, *held, left))
    .collect();
  while left > 0 {
    let Some(top) = heap.pop() else {
      break;
    };
    let Some(want) = wants.get(top.at) else {
      continue;
    };
    let Some(held) = given.get(top.at).copied() else {
      continue;
    };
    if top.bytes > left {
      // Its best no longer fits what is left: asked again within it.
      heap.extend(best(want, top.at, held, left));
      continue;
    }
    let now = held.saturating_add(top.bytes);
    if let Some(slot) = given.get_mut(top.at) {
      *slot = now;
    }
    left = left.saturating_sub(top.bytes);
    heap.extend(best(want, top.at, now, left));
  }
  Division { given, spare: left }
}

/// The bytes `curve` can use (module docs, "The floor"): its working set, no further than its last bin edge.
fn usable(curve: &Curve) -> u64 {
  curve.edges().last().unwrap_or(0).min(curve.working_set())
}

/// Repository `at`'s best extension from `held` bytes within `left` (module docs): `None` when no extension saves
/// anything.
fn best(want: &Want<'_>, at: usize, held: u64, left: u64) -> Option<Best> {
  if !(want.per_s.is_finite() && want.per_s > 0.0) || left == 0 {
    return None;
  }
  let here = want.curve.misses(held);
  let mut ends: Vec<u64> = want
    .curve
    .edges()
    .filter(|edge| *edge > held && edge.saturating_sub(held) <= left)
    .collect();
  // Where the curve still falls past the room left, the room left itself is an end (the line to the next edge).
  if want
    .curve
    .edges()
    .any(|edge| edge.saturating_sub(held) > left)
  {
    ends.push(held.saturating_add(left));
  }
  ends
    .into_iter()
    .filter_map(|end| {
      let bytes = end.saturating_sub(held);
      let saved = (here - want.curve.misses(end)) * want.per_s;
      let per_byte = saved / crate::share::amount_of(bytes);
      (bytes > 0 && per_byte.is_finite() && per_byte > 0.0).then_some(Best {
        per_byte,
        bytes,
        at,
      })
    })
    .max()
}

#[cfg(test)]
#[path = "partition/tests.rs"]
mod tests;
