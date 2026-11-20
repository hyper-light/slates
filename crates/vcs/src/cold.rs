//! What a repository over its memory share lets go first (A-724 piece 4; §4.17.5 "The cold trigger"; §4.11 "Cost
//! model").
//!
//! **What it is.** A repository holding more than its share of its shard (A-724 piece 3, `crate::share`) must give room
//! back, and RAM only (R1) leaves three ways: drop what can be made again (derived values and caches: a version's
//! encoded pack fragments, reach sets), compress what is cold, or hold cold content only at its homes on peers. This
//! module decides the first: which of a repository's rebuildable values to drop to cover its overrun, by §4.11's cost
//! model. Dropping a value saves its bytes, each worth `value_of_byte`; it costs its rebuild, paid on each read that
//! finds it gone before it is kept again, each nanosecond worth `value_of_cpu`. Taken in nanoseconds of this machine's
//! CPU (`value_of_cpu` = 1), a value is worth dropping when
//!
//! `bytes × ns_per_byte × pressure > rebuild_ns × reads`
//!
//! where `ns_per_byte` is what holding a byte is worth at neutral pressure (the owner passes the time this machine takes
//! to copy one, its measured memcpy throughput, as `crate::vcs_store::upload::exchange` prices a byte for the fragments'
//! encoder), `pressure` is how far past its share the repository is (what it holds over its share, at least one), and
//! `reads` the reads expected before the value would be kept again (the owner's measure from the repository's usage
//! row). The values worth dropping are taken by their saving a byte, highest first, until the overrun is covered: the
//! greedy order for a fractional knapsack, optimal when values may be split and within one value's bytes of optimal
//! when they may not [A: Dantzig, "Discrete-Variable Extremum Problems", Operations Research 1957].
//!
//! **Invariants** (held by the oracle in the tests): nothing is chosen whose saving is not positive; what is chosen is a
//! prefix of the worth-dropping values in that order; it covers the overrun when they can, and is the shortest such
//! prefix, so nothing is dropped past the overrun but the one value that crosses it; an overrun of zero drops nothing.

/// One value a repository could drop and make again (module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rebuildable {
  /// The bytes dropping it gives back.
  pub bytes: u64,
  /// The measured time making it again takes, in nanoseconds of this machine's CPU.
  pub rebuild_ns: u64,
  /// The reads expected to find it gone before it would be kept again (at least zero; not-a-number reads as zero).
  pub reads: f64,
}

/// The prices the cost model weighs a byte against a rebuild with (module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Prices {
  /// What holding one byte is worth at neutral pressure, in nanoseconds of this machine's CPU.
  pub ns_per_byte: f64,
  /// How far past its share the repository is: what it holds over its share, at least one.
  pub pressure: f64,
}

/// A number at least zero, or zero.
fn at_least_zero(value: f64) -> f64 {
  if value.is_finite() && value > 0.0 {
    value
  } else {
    0.0
  }
}

/// What dropping `value` saves at `prices`, in nanoseconds (module docs): its bytes' worth less its rebuilds' cost.
pub fn saving(value: &Rebuildable, prices: Prices) -> f64 {
  let worth = crate::share::amount_of(value.bytes)
    * at_least_zero(prices.ns_per_byte)
    * at_least_zero(prices.pressure).max(1.0);
  let cost = crate::share::amount_of(value.rebuild_ns) * at_least_zero(value.reads);
  worth - cost
}

/// The values of `candidates` to drop to give back `overrun` bytes at `prices` (module docs), as indexes into
/// `candidates`, in the order to drop them.
pub fn choose(overrun: u64, candidates: &[Rebuildable], prices: Prices) -> Vec<usize> {
  if overrun == 0 {
    return Vec::new();
  }
  let mut worth: Vec<(usize, f64)> = candidates
    .iter()
    .enumerate()
    .filter(|(_, value)| value.bytes > 0)
    .map(|(at, value)| (at, saving(value, prices)))
    .filter(|(_, saved)| *saved > 0.0)
    .map(|(at, saved)| {
      let bytes = candidates
        .get(at)
        .map_or(1.0, |value| crate::share::amount_of(value.bytes));
      (at, saved / bytes)
    })
    .collect();
  worth.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
  let mut chosen = Vec::new();
  let mut freed: u64 = 0;
  for (at, _) in worth {
    if freed >= overrun {
      break;
    }
    freed = freed.saturating_add(candidates.get(at).map_or(0, |value| value.bytes));
    chosen.push(at);
  }
  chosen
}

#[cfg(test)]
mod tests {
  use super::{Prices, Rebuildable, choose, saving};
  use proptest::prelude::*;

  fn candidate() -> impl Strategy<Value = Rebuildable> {
    (0u64..100_000, 0u64..1_000_000, 0u32..50).prop_map(|(bytes, rebuild_ns, reads)| Rebuildable {
      bytes,
      rebuild_ns,
      reads: f64::from(reads),
    })
  }

  /// The serial reference: every value worth dropping, ordered by its saving a byte and then its index, taken one by
  /// one while what was taken is short of the overrun.
  fn reference(overrun: u64, candidates: &[Rebuildable], prices: Prices) -> Vec<usize> {
    if overrun == 0 {
      return Vec::new();
    }
    let mut order: Vec<usize> = (0..candidates.len())
      .filter(|at| candidates[*at].bytes > 0 && saving(&candidates[*at], prices) > 0.0)
      .collect();
    order.sort_by(|a, b| {
      let ra = saving(&candidates[*a], prices) / candidates[*a].bytes as f64;
      let rb = saving(&candidates[*b], prices) / candidates[*b].bytes as f64;
      rb.total_cmp(&ra).then(a.cmp(b))
    });
    let mut taken = Vec::new();
    let mut freed = 0u64;
    for at in order {
      if freed >= overrun {
        break;
      }
      freed += candidates[at].bytes;
      taken.push(at);
    }
    taken
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(500)))]

    /// A-724 piece 4 (§4.17.5 "The cold trigger"; §4.11 "Cost model"): do any overrun over up to sixteen rebuildable
    /// values at any prices, expect the reference's choice; every chosen value saving more than it costs; the overrun
    /// covered whenever the values worth dropping cover it; and no shorter prefix covering it, so nothing is dropped
    /// past the overrun but the value that crosses it.
    #[test]
    fn a_choice_equals_the_reference_and_drops_no_more_than_the_overrun_needs(
      overrun in 0u64..400_000,
      candidates in proptest::collection::vec(candidate(), 0..16),
      ns_per_byte in 0u32..64,
      pressure in 1u32..8,
    ) {
      let prices = Prices { ns_per_byte: f64::from(ns_per_byte), pressure: f64::from(pressure) };
      let chosen = choose(overrun, &candidates, prices);
      prop_assert_eq!(&chosen, &reference(overrun, &candidates, prices));
      for at in &chosen {
        prop_assert!(saving(&candidates[*at], prices) > 0.0);
      }
      let freed: u64 = chosen.iter().map(|at| candidates[*at].bytes).sum();
      let worth: u64 = candidates
        .iter()
        .filter(|value| value.bytes > 0 && saving(value, prices) > 0.0)
        .map(|value| value.bytes)
        .sum();
      if overrun > 0 && worth >= overrun {
        prop_assert!(freed >= overrun, "the overrun {} not covered: {}", overrun, freed);
      }
      if let Some((_, before)) = chosen.split_last() {
        let short: u64 = before.iter().map(|at| candidates[*at].bytes).sum();
        prop_assert!(short < overrun, "a shorter prefix already covered the overrun");
      }
    }
  }

  /// A-724 piece 4: do two caches of equal size beside a repository over its share, one read often and costly to make
  /// again, the other rarely read, expect the rarely read one dropped first; and with the pressure raised far enough,
  /// expect the costly one dropped too, as §4.11's model raises a byte's value under pressure.
  #[test]
  fn a_rarely_read_cache_goes_first_and_pressure_lets_a_costly_one_go_too() {
    let hot = Rebuildable {
      bytes: 1 << 20,
      rebuild_ns: 50_000_000,
      reads: 20.0,
    };
    let cold = Rebuildable {
      bytes: 1 << 20,
      rebuild_ns: 50_000_000,
      reads: 0.0,
    };
    let neutral = Prices {
      ns_per_byte: 0.1,
      pressure: 1.0,
    };
    assert_eq!(choose(1 << 20, &[hot, cold], neutral), vec![1]);
    assert_eq!(choose(2 << 20, &[hot, cold], neutral), vec![1]);
    let pressed = Prices {
      ns_per_byte: 0.1,
      pressure: 10_000.0,
    };
    assert_eq!(choose(2 << 20, &[hot, cold], pressed), vec![1, 0]);
  }

  /// A-724 piece 4: do an overrun of zero, and prices that are not numbers, expect nothing dropped.
  #[test]
  fn no_overrun_or_no_price_drops_nothing() {
    let value = Rebuildable {
      bytes: 10,
      rebuild_ns: 0,
      reads: 0.0,
    };
    assert!(
      choose(
        0,
        &[value],
        Prices {
          ns_per_byte: 1.0,
          pressure: 1.0
        }
      )
      .is_empty()
    );
    assert!(
      choose(
        5,
        &[value],
        Prices {
          ns_per_byte: f64::NAN,
          pressure: 1.0
        }
      )
      .is_empty()
    );
  }
}
