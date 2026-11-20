//! The shares of a shard's resources among its repositories (A-724 piece 2; §4.17.5 "Shares").
//!
//! **What it is.** Each repository with work on a shard claims a demand vector, what it would use of each resource if
//! nothing stood in its way (its memory and its CPU, measured from its usage row, `crate::usage`), and a weight. This
//! module divides the shard's capacity of each resource among the claims by weighted dominant-resource fairness with
//! demands as caps [A: Ghodsi, Zaharia, Hindman, Konwinski, Shenker and Stoica, "Dominant Resource Fairness: Fair
//! Allocation of Multiple Resource Types", NSDI 2011]. A claim's *dominant share* is the largest fraction of any one
//! resource's capacity its allocation takes. DRF raises every claim's dominant share over its weight together (the
//! paper's progressive filling) until a resource is full or the claim has all it asked; claims still short then share
//! what is left the same way. The paper proves the result shares fairly (each of `n` equal claims gets at least
//! `1/n` of its dominant resource, or all it asked), is Pareto-efficient, envy-free and strategy-proof: a repository
//! gains nothing by overstating its demand. No share is fixed: every figure is its claim's measured demand and the
//! shard's measured capacity.
//!
//! **How it is solved.** A claim `i` with demand `D_i`, weight `w_i` and dominant demand `δ_i = max_r D_ir / C_r` gets
//! the fraction `f_i(L) = min(1, w_i L / δ_i)` of its demand at level `L`. Each resource's use `U_r(L) = Σ_i f_i(L) D_ir`
//! is continuous, nondecreasing and linear between the breakpoints `L = δ_i / w_i`, so the largest level no resource
//! overflows is found exactly: walk the breakpoints in order and solve the one linear piece that crosses a capacity.
//! Progressive filling then freezes the claims that use a full resource and goes on with the rest, at most once a
//! resource, so a division costs `O(resources × claims log claims)`.
//!
//! **Invariants** (held by the oracle in the tests): no resource is given past its capacity; no claim gets past its
//! demand; a claim given less than its demand uses a full resource (the division is work-conserving: room is left
//! idle only when every claim that could use it has all it asked); and every claim gets at least what equal division
//! of the level would give it, `min(1, w_i L* / δ_i)` at the first full resource's level `L*`.
//!
//! **What it is not.** It divides; it neither measures nor admits. The demands and weights are the owner's to read from
//! its usage rows, and admission by these shares is A-724's piece 3. Arithmetic is in `f64`, where no operation panics
//! (a division by zero is infinite, never a trap), and every result is clamped into its bounds before it is used.

/// One repository's claim on a shard's resources (module docs).
#[derive(Clone, Debug, PartialEq)]
pub struct Claim<const R: usize> {
  /// What it would use of each resource with nothing in its way, in that resource's own unit; a negative or not-a-number
  /// demand reads as zero.
  pub demand: [f64; R],
  /// Its weight: its dominant share grows in proportion to it. A weight that is not positive and finite claims nothing.
  pub weight: f64,
}

/// What one claim was given (module docs).
#[derive(Clone, Debug, PartialEq)]
pub struct Granted<const R: usize> {
  /// The fraction of its demand it was given, in `[0, 1]`.
  pub fraction: f64,
  /// What it was given of each resource: its demand times `fraction`.
  pub granted: [f64; R],
}

/// A demand or a capacity as this module reads it: a number at least zero, or zero.
fn amount(value: f64) -> f64 {
  if value.is_finite() && value > 0.0 {
    value
  } else {
    0.0
  }
}

/// A claim's dominant demand over `capacity`: the largest fraction of one resource's capacity its whole demand takes.
/// A demand on a resource of no capacity is infinite, so such a claim is given nothing of anything.
fn dominant<const R: usize>(demand: &[f64; R], capacity: &[f64; R]) -> f64 {
  demand
    .iter()
    .zip(capacity)
    .map(|(asked, held)| {
      let asked = amount(*asked);
      if asked == 0.0 {
        0.0
      } else {
        asked / amount(*held)
      }
    })
    .fold(0.0, f64::max)
}

/// One claim still being raised: its index, its demand, and the level at which it has all it asked.
struct Rising<const R: usize> {
  at: usize,
  demand: [f64; R],
  /// `δ_i / w_i`: the level at which its fraction reaches one.
  full_at: f64,
}

/// Divides `capacity` among `claims` by weighted dominant-resource fairness with demands as caps (module docs): one
/// grant a claim, in the claims' order.
pub fn divide<const R: usize>(capacity: &[f64; R], claims: &[Claim<R>]) -> Vec<Granted<R>> {
  let mut grants: Vec<Granted<R>> = claims
    .iter()
    .map(|_| Granted {
      fraction: 0.0,
      granted: [0.0; R],
    })
    .collect();
  let mut left: [f64; R] = capacity.map(amount);
  let mut rising: Vec<Rising<R>> = Vec::with_capacity(claims.len());
  for (at, claim) in claims.iter().enumerate() {
    let demand = claim.demand.map(amount);
    let weight = amount(claim.weight);
    let dominant_demand = dominant(&demand, capacity);
    if weight == 0.0 || !dominant_demand.is_finite() {
      continue;
    }
    if dominant_demand == 0.0 {
      if let Some(grant) = grants.get_mut(at) {
        grant.fraction = 1.0;
      }
      continue;
    }
    rising.push(Rising {
      at,
      demand,
      full_at: dominant_demand / weight,
    });
  }
  // Progressive filling: each pass raises every rising claim to the largest level no resource overflows, settles the
  // claims that reached their demand or use a resource that is now full, and goes on with the rest. Each pass fills a
  // resource or settles every claim, so it ends within one pass a resource and one more.
  let mut passes_left = R.saturating_add(1);
  while !rising.is_empty() && passes_left > 0 {
    passes_left = passes_left.saturating_sub(1);
    rising.sort_by(|left, right| left.full_at.total_cmp(&right.full_at));
    let level = largest_level(&left, &rising);
    let mut full = [false; R];
    for (resource, flag) in full.iter_mut().enumerate() {
      let used: f64 = rising
        .iter()
        .map(|claim| fraction(claim, level) * claim.demand.get(resource).copied().unwrap_or(0.0))
        .sum();
      let room = left.get(resource).copied().unwrap_or(0.0);
      *flag = used >= room * (1.0 - f64::EPSILON * 64.0) && used > 0.0;
    }
    let mut still: Vec<Rising<R>> = Vec::with_capacity(rising.len());
    for claim in rising {
      let given = fraction(&claim, level);
      let uses_full = claim
        .demand
        .iter()
        .zip(&full)
        .any(|(asked, full)| *full && *asked > 0.0);
      if given >= 1.0 || uses_full {
        settle(&mut grants, &mut left, &claim, given);
      } else {
        still.push(claim);
      }
    }
    rising = still;
  }
  for claim in rising {
    let given = fraction(&claim, largest_level(&left, core::slice::from_ref(&claim)));
    settle(&mut grants, &mut left, &claim, given);
  }
  grants
}

/// A rising claim's fraction at `level`: `min(1, level / full_at)`.
fn fraction<const R: usize>(claim: &Rising<R>, level: f64) -> f64 {
  if claim.full_at <= 0.0 {
    return 1.0;
  }
  (level / claim.full_at).clamp(0.0, 1.0)
}

/// Gives `claim` the fraction `given` of its demand, taking it from what is `left`.
fn settle<const R: usize>(
  grants: &mut [Granted<R>],
  left: &mut [f64; R],
  claim: &Rising<R>,
  given: f64,
) {
  let given = given.clamp(0.0, 1.0);
  let granted = claim.demand.map(|asked| asked * given);
  for (room, taken) in left.iter_mut().zip(&granted) {
    *room = (*room - taken).max(0.0);
  }
  if let Some(grant) = grants.get_mut(claim.at) {
    *grant = Granted {
      fraction: given,
      granted,
    };
  }
}

/// The largest level at which no resource's use by the `rising` claims (sorted by `full_at`) passes what is `left` of
/// it (module docs, "How it is solved"); the last breakpoint when none would.
fn largest_level<const R: usize>(left: &[f64; R], rising: &[Rising<R>]) -> f64 {
  let mut best = rising.last().map_or(0.0, |claim| claim.full_at);
  for (resource, room) in left.iter().enumerate() {
    if let Some(level) = level_filling(*room, resource, rising) {
      best = best.min(level);
    }
  }
  best.max(0.0)
}

/// The level at which `rising`'s use of `resource` reaches `room`, or none when it never does. Between breakpoints the
/// use is `full + level × slope`: `full` the demand of the claims already whole, `slope` the sum of `demand / full_at`
/// of the rest.
fn level_filling<const R: usize>(room: f64, resource: usize, rising: &[Rising<R>]) -> Option<f64> {
  let demand_of = |claim: &Rising<R>| claim.demand.get(resource).copied().unwrap_or(0.0);
  let mut slope: f64 = rising
    .iter()
    .map(|claim| demand_of(claim) / claim.full_at)
    .sum();
  let mut whole = 0.0;
  let mut from = 0.0;
  for claim in rising {
    // On the piece [from, claim.full_at] the use is whole + level × slope. With no slope the use stands still, and it
    // was within the room where the piece began (the use is continuous), so a flat piece never reaches the room: a
    // resource no rising claim uses never stops the level, however little of it is left.
    if slope > 0.0 {
      let level = (room - whole) / slope;
      if level <= claim.full_at {
        return Some(level.max(from));
      }
    }
    let asked = demand_of(claim);
    whole += asked;
    slope = (slope - asked / claim.full_at).max(0.0);
    from = claim.full_at;
  }
  None
}

/// An amount this module computed, as whole units for a caller that counts in integers (a byte count): rounded down,
/// a value that is not a number or not positive reads as zero and one past `u64::MAX` as `u64::MAX`.
pub fn whole_units(value: f64) -> u64 {
  if value.is_nan() || value <= 0.0 {
    return 0;
  }
  if value >= u64::MAX as f64 {
    return u64::MAX;
  }
  // The value is finite, positive and below the integer's top (both checked just above).
  #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
  let whole = value as u64;
  whole
}

/// A count in integers (a byte count) as this module's amount: exact below 2^53, the nearest `f64` above, where a
/// share's last bytes never decide anything.
pub fn amount_of(units: u64) -> f64 {
  units as f64
}

#[cfg(test)]
mod tests {
  use super::{Claim, divide};
  use proptest::prelude::*;

  const RESOURCES: usize = 2;

  /// The relative slack every comparison allows: the module works in `f64`.
  fn close(left: f64, right: f64) -> bool {
    (left - right).abs() <= 1e-6 * (1.0 + left.abs().max(right.abs()))
  }

  /// The serial reference: progressive filling by bisection on the level, pass by pass, each claim's fraction
  /// `min(1, w L / δ)`; a claim is frozen once a resource it uses is full. It shares nothing with the module but the
  /// rule's statement.
  fn reference(capacity: &[f64; RESOURCES], claims: &[Claim<RESOURCES>]) -> Vec<f64> {
    let dominant = |claim: &Claim<RESOURCES>| {
      claim
        .demand
        .iter()
        .zip(capacity)
        .map(|(asked, held)| if *asked == 0.0 { 0.0 } else { asked / held })
        .fold(0.0, f64::max)
    };
    let mut fraction = vec![0.0; claims.len()];
    let mut frozen: Vec<bool> = claims.iter().map(|claim| dominant(claim) == 0.0).collect();
    for (at, claim) in claims.iter().enumerate() {
      if dominant(claim) == 0.0 {
        fraction[at] = 1.0;
      }
    }
    let mut left = *capacity;
    for _ in 0..=RESOURCES {
      let open: Vec<usize> = (0..claims.len()).filter(|at| !frozen[*at]).collect();
      if open.is_empty() {
        break;
      }
      // A fraction within rounding of one is one: at the level `δ / w` the product `w (δ / w) / δ` may round below it.
      let at_level = |level: f64, at: usize| {
        let given = claims[at].weight * level / dominant(&claims[at]);
        if given >= 1.0 - 1e-12 { 1.0 } else { given }
      };
      let fits = |level: f64| {
        (0..RESOURCES).all(|r| {
          open
            .iter()
            .map(|at| at_level(level, *at) * claims[*at].demand[r])
            .sum::<f64>()
            <= left[r] * (1.0 + 1e-12)
        })
      };
      let top = open
        .iter()
        .map(|at| dominant(&claims[*at]) / claims[*at].weight)
        .fold(0.0, f64::max);
      let (mut low, mut high) = (0.0, top);
      if fits(top) {
        low = top;
      } else {
        for _ in 0..200 {
          let middle = (low + high) / 2.0;
          if fits(middle) {
            low = middle
          } else {
            high = middle
          }
        }
      }
      let full: Vec<bool> = (0..RESOURCES)
        .map(|r| {
          let used: f64 = open
            .iter()
            .map(|at| at_level(low, *at) * claims[*at].demand[r])
            .sum();
          used > 0.0 && close(used, left[r])
        })
        .collect();
      for at in &open {
        let given = at_level(low, *at);
        let uses_full = (0..RESOURCES).any(|r| full[r] && claims[*at].demand[r] > 0.0);
        if given >= 1.0 || uses_full {
          fraction[*at] = given;
          frozen[*at] = true;
          for r in 0..RESOURCES {
            left[r] = (left[r] - given * claims[*at].demand[r]).max(0.0);
          }
        }
      }
    }
    fraction
  }

  fn claim() -> impl Strategy<Value = Claim<RESOURCES>> {
    (0u32..1000, 0u32..1000, 1u32..8).prop_map(|(memory, cpu, weight)| Claim {
      demand: [f64::from(memory), f64::from(cpu)],
      weight: f64::from(weight),
    })
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(500)))]

    /// A-724 piece 2 (§4.17.5 "Shares"): do any division of two resources among up to twelve weighted claims, expect
    /// the serial reference's fractions; no resource past its capacity and no claim past its demand; every claim given
    /// less than its demand using a full resource (work-conserving); and no claim given less than its weighted equal
    /// part of the first full resource's level, the paper's sharing incentive.
    #[test]
    fn a_division_equals_the_reference_and_holds_drfs_properties(
      capacity in (1u32..2000, 1u32..2000),
      claims in proptest::collection::vec(claim(), 0..12),
    ) {
      let capacity = [f64::from(capacity.0), f64::from(capacity.1)];
      let grants = divide(&capacity, &claims);
      prop_assert_eq!(grants.len(), claims.len());
      let expected = reference(&capacity, &claims);
      for (grant, want) in grants.iter().zip(&expected) {
        prop_assert!(close(grant.fraction, *want), "fraction {} against the reference's {}", grant.fraction, want);
      }
      let mut used = [0.0; RESOURCES];
      for (grant, claim) in grants.iter().zip(&claims) {
        prop_assert!((0.0..=1.0).contains(&grant.fraction));
        for r in 0..RESOURCES {
          prop_assert!(grant.granted[r] <= claim.demand[r] * (1.0 + 1e-9));
          used[r] += grant.granted[r];
        }
      }
      let full: Vec<bool> = (0..RESOURCES).map(|r| close(used[r], capacity[r]) || used[r] > capacity[r]).collect();
      for r in 0..RESOURCES {
        prop_assert!(used[r] <= capacity[r] * (1.0 + 1e-9), "resource {} given {} of {}", r, used[r], capacity[r]);
      }
      for (grant, claim) in grants.iter().zip(&claims) {
        if grant.fraction < 1.0 - 1e-9 {
          prop_assert!(
            (0..RESOURCES).any(|r| full[r] && claim.demand[r] > 0.0),
            "a claim left short while every resource it uses has room: {:?}", claim
          );
        }
      }
      // Sharing incentive with equal weights: each of n claims gets at least min(all it asked, 1/n of its dominant
      // resource).
      if claims.iter().all(|claim| claim.weight == claims[0].weight) && !claims.is_empty() {
        let parts = claims.len() as f64;
        for (grant, claim) in grants.iter().zip(&claims) {
          let dominant = (0..RESOURCES)
            .map(|r| if claim.demand[r] == 0.0 { 0.0 } else { claim.demand[r] / capacity[r] })
            .fold(0.0, f64::max);
          let floor = if dominant == 0.0 { 1.0 } else { (1.0 / parts / dominant).min(1.0) };
          prop_assert!(grant.fraction >= floor - 1e-6, "a claim got {} below its equal part {}", grant.fraction, floor);
        }
      }
    }
  }

  /// A-724 piece 2: do a toy repository's small claim beside a monorepo's claim of ten times the shard, expect the toy
  /// given all it asked and the monorepo the rest of the full resource, never past it.
  #[test]
  fn a_monorepo_never_starves_a_toy_repository() {
    let capacity = [1000.0, 1000.0];
    let toy = Claim {
      demand: [10.0, 5.0],
      weight: 1.0,
    };
    let monorepo = Claim {
      demand: [10_000.0, 2_000.0],
      weight: 1.0,
    };
    let grants = divide(&capacity, &[toy, monorepo]);
    assert_eq!(grants[0].fraction, 1.0);
    assert!(close(grants[0].granted[0] + grants[1].granted[0], 1000.0));
    assert!(grants[1].granted[1] <= 1000.0);
  }

  /// A-724 piece 2: do one claim heavy in memory and one heavy in CPU, expect each given the most of its dominant
  /// resource while their dominant shares stay equal, as the paper's own example divides 9 CPUs and 18 GB between
  /// demands of <1 CPU, 4 GB> and <3 CPUs, 1 GB> a task: three tasks and two.
  #[test]
  fn the_papers_example_divides_as_the_paper_does() {
    let capacity = [9.0, 18.0];
    // Demands for many tasks, so the caps never bind: a hundred tasks each.
    let memory_heavy = Claim {
      demand: [100.0, 400.0],
      weight: 1.0,
    };
    let cpu_heavy = Claim {
      demand: [300.0, 100.0],
      weight: 1.0,
    };
    let grants = divide(&capacity, &[memory_heavy, cpu_heavy]);
    assert!(
      close(grants[0].granted[0], 3.0) && close(grants[0].granted[1], 12.0),
      "{grants:?}"
    );
    assert!(
      close(grants[1].granted[0], 6.0) && close(grants[1].granted[1], 2.0),
      "{grants:?}"
    );
  }

  /// A-724 piece 2: do claims with a weight of zero, a demand on a resource of no capacity, and no demand at all, expect
  /// nothing for the first two and all asked for the last, and no grant anywhere that is not a number.
  #[test]
  fn degenerate_claims_are_given_nothing_or_all_and_never_not_a_number() {
    let capacity = [100.0, 0.0];
    let claims = [
      Claim {
        demand: [50.0, 0.0],
        weight: 0.0,
      },
      Claim {
        demand: [10.0, 1.0],
        weight: 1.0,
      },
      Claim {
        demand: [0.0, 0.0],
        weight: 1.0,
      },
      Claim {
        demand: [f64::NAN, -3.0],
        weight: f64::INFINITY,
      },
    ];
    let grants = divide(&capacity, &claims);
    assert_eq!(grants[0].fraction, 0.0);
    assert_eq!(grants[1].fraction, 0.0);
    assert_eq!(grants[2].fraction, 1.0);
    assert!(
      grants
        .iter()
        .all(|grant| grant.fraction.is_finite() && grant.granted.iter().all(|g| g.is_finite()))
    );
  }
}
