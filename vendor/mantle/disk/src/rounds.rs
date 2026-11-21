//! Judging the rounds of a measured point (docs/design/measurement.md; docs/research/21 §9).
//!
//! A benchmark point runs in rounds, one throughput each. Every interval stated here assumes
//! independent rounds from one distribution: "if it does not hold, the confidence intervals are
//! wrong" (Le Boudec, *Performance Evaluation of Computer and Communication Systems*, 2010,
//! §2.2.2). So the rounds are first checked for order in time, with the lag-1 autocorrelation
//! against `±1.96/√n` (§2.3.2) and the exact runs test above and below the median
//! (NIST/SEMATECH e-Handbook §1.3.5.13). Hartigan and Hartigan's dip test (Ann. Statist. 13(1),
//! 1985) then decides whether they fall in one state or two, against the percentage points of
//! its Table 1, and two states are split where the within-group sum of squares is least. Each
//! state is summarized by its median, whose interval is the order statistics of Le Boudec's
//! Theorem 2.1, and by its share of the rounds, whose interval is his Theorem 2.4. A mean and a
//! coefficient of variation describe neither of two states (Hoefler and Belli, SC '15, Fig. 3).

/// The most rounds a point may run. The exact binomial sums of the median's interval and of
/// the runs test are counted in `u128`, where `40 · 2^n` fits for `n ≤ 120`.
pub const MAX_ROUNDS: usize = 120;

/// How a point's rounds are ordered in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Fewer than ten rounds, where the runs test has no two-sided 5% rejection region
    /// (docs/research/21 §4), or more than [`MAX_ROUNDS`].
    Untested,
    Independent,
    /// Rounds stay in a state: fewer runs than chance allows, or a lag-1 correlation above its
    /// bound.
    Persistent,
    /// Rounds alternate: more runs than chance allows, or a lag-1 correlation below its bound.
    Alternating,
}

/// Rounds that fall in one state.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    /// The rounds, as their indices in time order.
    pub rounds: Vec<usize>,
    pub median: f64,
    /// The median's 95% interval: present when the rounds are independent and the state has
    /// at least six, the fewest for which one exists (Le Boudec, Table A.1).
    pub interval: Option<(f64, f64)>,
    pub min: f64,
    pub max: f64,
}

impl State {
    /// The interval's wider side as a fraction of the median.
    pub fn spread(&self) -> Option<f64> {
        let (lo, hi) = self.interval?;
        (self.median > 0.0).then(|| (self.median - lo).max(hi - self.median) / self.median)
    }
}

/// What a point's rounds show.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgment {
    pub rounds: usize,
    pub order: Order,
    /// The lag-1 autocorrelation (NIST §1.3.5.12).
    pub lag1: f64,
    /// Runs above and below the median, when rounds lie on both sides of it.
    pub runs: Option<usize>,
    /// The dip and its 5% critical value, from four rounds.
    pub dip: Option<(f64, f64)>,
    /// One state, or two with the higher median first.
    pub states: Vec<State>,
}

/// When a point has run enough rounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    /// Rounds before the rounds are judged.
    pub min: usize,
    /// Rounds at most, within [`MAX_ROUNDS`].
    pub max: usize,
    /// The widest interval a state may end with, as a fraction of its median.
    pub precision: f64,
}

impl Policy {
    /// At least ten rounds, the fewest at which the runs test can find rounds dependent; at most
    /// thirty, enough for each of two states to carry its own interval when the smaller holds
    /// a fifth of the rounds, and for the dip test to see it (docs/research/21 §9.5). The
    /// precision, ±5%, is half the ±10% that comparisons between benchmark runs resolve
    /// (docs/research/11 §16).
    pub const STANDARD: Self = Self {
        min: 10,
        max: 30,
        precision: 0.05,
    };

    /// The most rounds a point runs.
    pub fn limit(&self) -> usize {
        self.max.max(self.min).clamp(1, MAX_ROUNDS)
    }

    /// Whether the rounds judged are enough: the limit reached, or at least the minimum, found
    /// independent, with every state's interval within the precision.
    pub fn enough(&self, judged: &Judgment) -> bool {
        judged.rounds >= self.limit()
            || (judged.rounds >= self.min
                && judged.order == Order::Independent
                && judged
                    .states
                    .iter()
                    .all(|s| s.spread().is_some_and(|w| w <= self.precision)))
    }
}

/// Judges `xs`, one value a round in time order.
pub fn judge(xs: &[f64]) -> Judgment {
    let n = xs.len();
    let lag1 = lag1(xs);
    let runs = runs(xs);
    let bound = 1.96 / f64::from(u32::try_from(n).unwrap_or(u32::MAX).max(1)).sqrt();
    let order = if !(10..=MAX_ROUNDS).contains(&n) {
        Order::Untested
    } else {
        let (few, many) = runs.map_or((false, false), |r| (r.few, r.many));
        let persistent = few || lag1 > bound;
        let alternating = many || lag1 < -bound;
        match (persistent, alternating) {
            (false, false) => Order::Independent,
            (true, false) => Order::Persistent,
            (false, true) => Order::Alternating,
            (true, true) if lag1 >= 0.0 => Order::Persistent,
            (true, true) => Order::Alternating,
        }
    };
    let mut sorted = xs.to_vec();
    sorted.sort_by(f64::total_cmp);
    let dip = critical(n).map(|c| (dip(&sorted), c));
    let independent = order == Order::Independent;
    let states = match (dip, cut(&sorted)) {
        (Some((d, c)), Some(threshold)) if d > c => {
            let (low, high): (Vec<usize>, Vec<usize>) =
                (0..n).partition(|&i| xs.get(i).is_some_and(|x| x.total_cmp(&threshold).is_le()));
            vec![state(xs, high, independent), state(xs, low, independent)]
        }
        _ => vec![state(xs, (0..n).collect(), independent)],
    };
    Judgment {
        rounds: n,
        order,
        lag1,
        runs: runs.map(|r| r.count),
        dip,
        states,
    }
}

fn state(xs: &[f64], rounds: Vec<usize>, independent: bool) -> State {
    let mut values: Vec<f64> = rounds.iter().filter_map(|&i| xs.get(i).copied()).collect();
    values.sort_by(f64::total_cmp);
    let interval = if independent {
        median_ranks(values.len()).and_then(|(j, k)| {
            Some((
                *values.get(j.checked_sub(1)?)?,
                *values.get(k.checked_sub(1)?)?,
            ))
        })
    } else {
        None
    };
    State {
        rounds,
        median: median(&values),
        interval,
        min: values.first().copied().unwrap_or(0.0),
        max: values.last().copied().unwrap_or(0.0),
    }
}

/// The median of `sorted`, the mean of the middle two when their count is even.
fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    let upper = sorted.get(n / 2).copied().unwrap_or(0.0);
    if n % 2 == 1 {
        return upper;
    }
    let lower = n
        .checked_sub(1)
        .and_then(|i| sorted.get(i / 2))
        .copied()
        .unwrap_or(upper);
    (lower + upper) / 2.0
}

/// The lag-1 autocorrelation `Σ (x_t − x̄)(x_{t+1} − x̄) / Σ (x_t − x̄)²`, zero when the values
/// do not vary.
fn lag1(xs: &[f64]) -> f64 {
    let n = f64::from(u32::try_from(xs.len()).unwrap_or(u32::MAX).max(1));
    let mean = xs.iter().sum::<f64>() / n;
    let spread: f64 = xs.iter().map(|x| (x - mean) * (x - mean)).sum();
    if spread <= 0.0 {
        return 0.0;
    }
    let lagged: f64 = xs
        .iter()
        .zip(xs.iter().skip(1))
        .map(|(a, b)| (a - mean) * (b - mean))
        .sum();
    lagged / spread
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Runs {
    count: usize,
    /// In the lower 2.5% tail: rounds persist.
    few: bool,
    /// In the upper 2.5% tail: rounds alternate.
    many: bool,
}

/// The runs above and below the median of `xs` in time order, rounds equal to the median left
/// out, and where the count falls in its exact distribution for independent rounds. With `a`
/// rounds above and `b` below, `P(R = 2k) ∝ 2·C(a−1, k−1)·C(b−1, k−1)` and
/// `P(R = 2k+1) ∝ C(a−1, k)·C(b−1, k−1) + C(a−1, k−1)·C(b−1, k)` over `C(a+b, a)`
/// (docs/research/21 §4). None without rounds on both sides, or past [`MAX_ROUNDS`].
fn runs(xs: &[f64]) -> Option<Runs> {
    if xs.len() > MAX_ROUNDS {
        return None;
    }
    let mut sorted = xs.to_vec();
    sorted.sort_by(f64::total_cmp);
    let m = median(&sorted);
    let sides: Vec<bool> = xs
        .iter()
        .filter(|x| x.total_cmp(&m).is_ne())
        .map(|&x| x > m)
        .collect();
    let above = sides.iter().filter(|&&s| s).count();
    let below = sides.len().checked_sub(above)?;
    if above == 0 || below == 0 {
        return None;
    }
    let count = sides
        .windows(2)
        .filter(|w| w.first() != w.get(1))
        .count()
        .checked_add(1)?;
    let (a, b) = (above.checked_sub(1)?, below.checked_sub(1)?);
    let ways = |r: usize| -> Option<u128> {
        let k = r / 2;
        if r.is_multiple_of(2) {
            let k1 = k.checked_sub(1)?;
            choose(a, k1)?.checked_mul(choose(b, k1)?)?.checked_mul(2)
        } else {
            let k1 = k.checked_sub(1)?;
            choose(a, k)?
                .checked_mul(choose(b, k1)?)?
                .checked_add(choose(a, k1)?.checked_mul(choose(b, k)?)?)
        }
    };
    let total = choose(above.checked_add(below)?, above)?;
    let most = above.checked_add(below)?;
    let (mut low, mut high) = (0u128, 0u128);
    for r in 2..=most {
        let w = ways(r).unwrap_or(0);
        if r <= count {
            low = low.checked_add(w)?;
        }
        if r >= count {
            high = high.checked_add(w)?;
        }
    }
    Some(Runs {
        count,
        few: low.checked_mul(40)? <= total,
        many: high.checked_mul(40)? <= total,
    })
}

/// `C(n, k)`, exact; None past `u128`.
fn choose(n: usize, k: usize) -> Option<u128> {
    if k > n {
        return Some(0);
    }
    let k = k.min(n.checked_sub(k)?);
    let n = u128::try_from(n).ok()?;
    let mut c = 1u128;
    for i in 0..u128::try_from(k).ok()? {
        // c·(n − i) is divisible by i + 1: c is C(n, i) times i!/i!, an integer at every step.
        c = c
            .checked_mul(n.checked_sub(i)?)?
            .checked_div(i.checked_add(1)?)?;
    }
    Some(c)
}

/// The ranks `(j, k)`, from 1, of the 95% interval `[x_(j), x_(k)]` of the median of `n`
/// independent rounds: the largest `j` whose tail `B(j−1)` below it holds at most 2.5%, and
/// `k = n + 1 − j`, so the coverage `B(k−1) − B(j−1)` of Le Boudec's Theorem 2.1 is at least
/// 95%, `B` the binomial CDF with `p = 1/2`. None below six rounds, where no interval reaches
/// 95% (his Table A.1), or past [`MAX_ROUNDS`].
pub fn median_ranks(n: usize) -> Option<(usize, usize)> {
    if n > MAX_ROUNDS {
        return None;
    }
    let total = 1u128.checked_shl(u32::try_from(n).ok()?)?;
    let mut below = 0u128;
    let mut j = 0usize;
    for i in 0..n {
        below = below.checked_add(choose(n, i)?)?;
        if below.checked_mul(40)? > total {
            break;
        }
        j = i.checked_add(1)?;
    }
    if j == 0 {
        return None;
    }
    Some((j, n.checked_add(1)?.checked_sub(j)?))
}

/// The 95% interval of the share `z/n` of rounds in a state, by Le Boudec's Theorem 2.4
/// (Clopper and Pearson's): the lower end `p` solves `P(Z ≥ z) = 2.5%`, the upper end
/// `P(Z ≤ z) = 2.5%`, `Z` binomial with `n` trials of probability `p`.
pub fn share(z: usize, n: usize) -> (f64, f64) {
    if n == 0 || z > n {
        return (0.0, 1.0);
    }
    let lower = if z == 0 {
        0.0
    } else {
        // B(z − 1) falls as p rises; find where it crosses 97.5%.
        solve(|p| binomial_cdf(z.saturating_sub(1), n, p) - 0.975)
    };
    let upper = if z == n {
        1.0
    } else {
        solve(|p| binomial_cdf(z, n, p) - 0.025)
    };
    (lower, upper)
}

/// The `p` in (0, 1) where a function falling in `p` crosses zero, by bisection to the
/// precision of an f64.
fn solve(f: impl Fn(f64) -> f64) -> f64 {
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    // Sixty-four halvings take the bracket below 2^-64, finer than an f64 near its ends.
    for _ in 0..64 {
        let mid = (lo + hi) / 2.0;
        if f(mid) > 0.0 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) / 2.0
}

/// `P(Z ≤ m)` for `Z` binomial with `n` trials of probability `p` in (0, 1), summed in
/// logarithms so no term underflows before it is added.
fn binomial_cdf(m: usize, n: usize, p: f64) -> f64 {
    let (lp, lq) = (p.ln(), (1.0 - p).ln());
    let nf = f64::from(u32::try_from(n).unwrap_or(u32::MAX));
    let mut log_term = nf * lq; // ln P(Z = 0)
    let mut sum = log_term.exp();
    for i in 0..m.min(n) {
        let i = f64::from(u32::try_from(i).unwrap_or(u32::MAX));
        log_term += ((nf - i) / (i + 1.0)).ln() + lp - lq;
        sum += log_term.exp();
    }
    sum.min(1.0)
}

/// Percentage points of the dip in uniform samples that 95% of dips fall below, from
/// Hartigan and Hartigan's Table 1 (9,999 samples each; standard error at most .001).
const DIP_95: [(usize, f64); 13] = [
    (4, 0.2056),
    (5, 0.1872),
    (6, 0.1645),
    (7, 0.1597),
    (8, 0.1552),
    (9, 0.1458),
    (10, 0.1394),
    (15, 0.1179),
    (20, 0.1047),
    (30, 0.0884),
    (50, 0.0702),
    (100, 0.0510),
    (200, 0.0370),
];

/// The dip above which `n` rounds form more than one state at the 5% level: Table 1's value,
/// or between its sample sizes interpolated linearly in `n` on `√n·dip`, as its note (4)
/// directs. None below four rounds or past 200.
pub fn critical(n: usize) -> Option<f64> {
    let root = |n: usize| f64::from(u32::try_from(n).unwrap_or(u32::MAX)).sqrt();
    DIP_95.windows(2).find_map(|w| {
        let (&(a, da), &(b, db)) = (w.first()?, w.get(1)?);
        if n == a {
            return Some(da);
        }
        if n == b {
            return Some(db);
        }
        if !(a < n && n < b) {
            return None;
        }
        let (sa, sb) = (root(a) * da, root(b) * db);
        let t = f64::from(u32::try_from(n.checked_sub(a)?).ok()?)
            / f64::from(u32::try_from(b.checked_sub(a)?).ok()?);
        Some((sa + (sb - sa) * t) / root(n))
    })
}

/// Hartigan and Hartigan's dip of `sorted`: the largest distance between the empirical
/// distribution function and the unimodal distribution function nearest it.
///
/// Their algorithm (§4, steps (i)–(vii)), in counts: observation `i` (from 0) has the lower
/// corner `(x_i, i)` and the upper corner `(x_i, i + 1)` of the empirical count function. Over
/// the current interval, `G` is the greatest convex minorant, the lower hull of the lower
/// corners, and `L` the least concave majorant, the upper hull of the upper corners. The
/// largest gap `d` between them at a point of contact marks a narrower interval; the distance
/// from `F` of `G` and `L` over the parts cut off raises `D`, and the search stops once `d ≤ D`,
/// with `2·dip = D/n`. Tied observations keep their own corners, so the jump at the mode may
/// divide them, as the definition allows. The interval shrinks every pass, so there are at most
/// `n` passes.
pub fn dip(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    match (sorted.first(), sorted.last()) {
        (Some(first), Some(last)) if first.total_cmp(last).is_lt() => {}
        // No observations, or all at one point: a point mass is unimodal.
        _ => return 0.0,
    }
    let x = |i: usize| sorted.get(i).copied().unwrap_or(0.0);
    let height = |i: usize, upper: bool| {
        f64::from(u32::try_from(i).unwrap_or(u32::MAX)) + if upper { 1.0 } else { 0.0 }
    };
    let (mut lo, mut hi) = (0usize, n.saturating_sub(1));
    let mut big_d = 0.0f64;
    for _ in 0..n {
        let g = hull(&x, &height, lo, hi, false);
        let l = hull(&x, &height, lo, hi, true);
        let gv = along(&x, &height, &g, false);
        let lv = along(&x, &height, &l, true);
        let at = |values: &[f64], i: usize| {
            i.checked_sub(lo)
                .and_then(|k| values.get(k))
                .copied()
                .unwrap_or(0.0)
        };
        // Steps (iii) and (iv): the largest gap between G and L at their points of contact,
        // and the interval it marks.
        let mut best: Option<(f64, usize, usize)> = None;
        for &v in &g {
            let d = at(&lv, v) - height(v, false);
            let next = l.iter().copied().find(|&w| w >= v).unwrap_or(hi);
            if best.is_none_or(|(b, _, _)| d > b) {
                best = Some((d, v, next));
            }
        }
        for &w in &l {
            let d = height(w, true) - at(&gv, w);
            let prev = g.iter().copied().rfind(|&v| v <= w).unwrap_or(lo);
            if best.is_none_or(|(b, _, _)| d >= b) {
                best = Some((d, prev, w));
            }
        }
        let Some((d, lo0, hi0)) = best else {
            break;
        };
        // Step (v).
        if d <= big_d {
            break;
        }
        // Step (vi): the parts of the interval cut off.
        let left = (lo..=lo0)
            .map(|i| height(i, true) - at(&gv, i))
            .fold(0.0f64, f64::max);
        let right = (hi0..=hi)
            .map(|i| at(&lv, i) - height(i, false))
            .fold(0.0f64, f64::max);
        big_d = big_d.max(left).max(right);
        // Step (vii).
        if (lo0, hi0) == (lo, hi) {
            break;
        }
        (lo, hi) = (lo0, hi0);
    }
    big_d / (2.0 * f64::from(u32::try_from(n).unwrap_or(u32::MAX)))
}

/// The lower hull of the lower corners (`upper` false) or the upper hull of the upper corners
/// (`upper` true) of observations `lo..=hi`, as their indices.
fn hull(
    x: &impl Fn(usize) -> f64,
    height: &impl Fn(usize, bool) -> f64,
    lo: usize,
    hi: usize,
    upper: bool,
) -> Vec<usize> {
    let mut h: Vec<usize> = Vec::with_capacity(hi.saturating_sub(lo).saturating_add(1));
    for k in lo..=hi {
        while let (Some(&a), Some(&b)) = (h.len().checked_sub(2).and_then(|i| h.get(i)), h.last()) {
            let cross = (x(b) - x(a)) * (height(k, upper) - height(a, upper))
                - (height(b, upper) - height(a, upper)) * (x(k) - x(a));
            let turns = if upper { cross < 0.0 } else { cross > 0.0 };
            if turns {
                break;
            }
            h.pop();
        }
        h.push(k);
    }
    h
}

/// A hull's value at every observation from its first vertex to its last. Between vertices at
/// the same point, a run of tied observations, the hull passes through each one's corner.
fn along(
    x: &impl Fn(usize) -> f64,
    height: &impl Fn(usize, bool) -> f64,
    hull: &[usize],
    upper: bool,
) -> Vec<f64> {
    let mut values = Vec::new();
    if let Some(&first) = hull.first() {
        values.push(height(first, upper));
    }
    for w in hull.windows(2) {
        let (Some(&a), Some(&b)) = (w.first(), w.get(1)) else {
            continue;
        };
        let (xa, xb) = (x(a), x(b));
        let (ya, yb) = (height(a, upper), height(b, upper));
        for i in a.saturating_add(1)..=b {
            values.push(if xb > xa {
                ya + (yb - ya) * (x(i) - xa) / (xb - xa)
            } else {
                height(i, upper)
            });
        }
    }
    values
}

/// Where two states part: the value below which the lower state's rounds lie, chosen where
/// the within-group sum of squares of the sorted values is least, and only between unequal
/// values so every round falls on one side. None when all values are equal.
fn cut(sorted: &[f64]) -> Option<f64> {
    let n = sorted.len();
    let total: f64 = sorted.iter().sum();
    let total_sq: f64 = sorted.iter().map(|x| x * x).sum();
    let (mut sum, mut sq) = (0.0f64, 0.0f64);
    let mut best: Option<(f64, f64)> = None;
    for c in 1..n {
        let (Some(&below), Some(&above)) = (sorted.get(c.checked_sub(1)?), sorted.get(c)) else {
            continue;
        };
        sum += below;
        sq += below * below;
        if below.total_cmp(&above).is_ge() {
            continue;
        }
        let left = f64::from(u32::try_from(c).ok()?);
        let right = f64::from(u32::try_from(n.checked_sub(c)?).ok()?);
        let within =
            (sq - sum * sum / left) + ((total_sq - sq) - (total - sum) * (total - sum) / right);
        if best.is_none_or(|(w, _)| within < w) {
            best = Some((within, below));
        }
    }
    best.map(|(_, threshold)| threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_ranks_hold_each_tail_to_two_and_a_half_percent() {
        // Le Boudec's example: ten results, the smallest and largest removed.
        assert_eq!(median_ranks(10), Some((2, 9)));
        // Rows of his Table A.1 where the equal-tailed ranks are his.
        for (n, jk) in [
            (6, (1, 6)),
            (12, (3, 10)),
            (13, (3, 11)),
            (20, (6, 15)),
            (30, (10, 21)),
        ] {
            assert_eq!(median_ranks(n), Some(jk), "n = {n}");
        }
        for n in 0..=5 {
            assert_eq!(median_ranks(n), None, "n = {n}");
        }
        assert_eq!(median_ranks(MAX_ROUNDS + 1), None);
        // Every n up to the limit: each tail at most 2.5%, and one more rank would pass it.
        for n in 6..=MAX_ROUNDS {
            let (j, k) = median_ranks(n).unwrap();
            assert_eq!(j + k, n + 1);
            let total = 1u128 << n;
            let tail: u128 = (0..j).map(|i| choose(n, i).unwrap()).sum();
            assert!(tail * 40 <= total, "n = {n}");
            assert!((tail + choose(n, j).unwrap()) * 40 > total, "n = {n}");
        }
    }

    #[test]
    fn share_intervals_match_le_boudecs_examples() {
        // Example 2.4: 32 of 145 readings, "L = 15.6%, U = 29.7%"; none of 10, "p ≤ 30.8%".
        let (lo, hi) = share(32, 145);
        assert!(
            (lo - 0.156).abs() < 5e-4 && (hi - 0.297).abs() < 5e-4,
            "{lo} {hi}"
        );
        let (lo, hi) = share(0, 10);
        assert_eq!(lo, 0.0);
        assert!((hi - 0.308).abs() < 5e-4, "{hi}");
        // Symmetric: U(z) = 1 − L(n − z).
        let (a, b) = share(7, 30);
        let (c, d) = share(23, 30);
        assert!((a - (1.0 - d)).abs() < 1e-12 && (b - (1.0 - c)).abs() < 1e-12);
        assert_eq!(share(30, 30).1, 1.0);
    }

    #[test]
    fn runs_tests_reject_exactly_where_the_distribution_says() {
        // Two-sided 5% regions for n/2 rounds on each side (docs/research/21 §4): n = 10
        // rejects R ≤ 2 or R ≥ 10, n = 20 R ≤ 6 or R ≥ 16, n = 30 R ≤ 10 or R ≥ 22.
        for (n, lower, upper) in [(10, 2, 10), (20, 6, 16), (30, 10, 22), (50, 18, 34)] {
            for r in 2..=n {
                let xs = with_runs(n, r);
                let got = runs(&xs).unwrap();
                assert_eq!(got.count, r);
                assert_eq!(got.few, r <= lower, "n = {n}, R = {r}");
                assert_eq!(got.many, r >= upper, "n = {n}, R = {r}");
            }
        }
        // Below ten rounds nothing is rejected.
        for r in 2..=8 {
            let got = runs(&with_runs(8, r)).unwrap();
            assert!(!got.few && !got.many);
        }
        assert_eq!(runs(&[3.0; 12]), None);
    }

    /// `n` rounds, half above the median and half below, in exactly `r` runs.
    fn with_runs(n: usize, r: usize) -> Vec<f64> {
        let half = n / 2;
        // Split each side into its runs: the high side takes ⌈r/2⌉ runs, the low ⌊r/2⌋.
        let parts = |count: usize, runs: usize| -> Vec<usize> {
            let mut v = vec![1; runs];
            v[0] += count - runs;
            v
        };
        let high = parts(half, r.div_ceil(2));
        let low = parts(half, r / 2);
        let mut xs = Vec::new();
        for i in 0..r {
            let (len, value) = if i % 2 == 0 {
                (high[i / 2], 2.0)
            } else {
                (low[i / 2], 1.0)
            };
            xs.extend(std::iter::repeat_n(value + i as f64 * 1e-3, len));
        }
        xs
    }

    #[test]
    fn critical_values_are_table_1_and_interpolate_on_root_n_dip() {
        assert_eq!(critical(3), None);
        assert_eq!(critical(4), Some(0.2056));
        assert_eq!(critical(30), Some(0.0884));
        assert_eq!(critical(100), Some(0.0510));
        // Between 10 and 15: √n·dip runs from √10·.1394 to √15·.1179.
        let s = 10f64.sqrt() * 0.1394 + (15f64.sqrt() * 0.1179 - 10f64.sqrt() * 0.1394) * 2.0 / 5.0;
        assert!((critical(12).unwrap() - s / 12f64.sqrt()).abs() < 1e-12);
        // Falling with n, as the table does.
        for n in 5..=MAX_ROUNDS {
            assert!(critical(n).unwrap() < critical(n - 1).unwrap(), "n = {n}");
        }
        assert_eq!(critical(201), None);
    }

    /// Dips of samples computed by R's diptest 0.77 (through its Python port, 0.11) and, for
    /// the tied samples, by a linear program over unimodal distribution functions from the
    /// definition; the two agree on every sample here.
    #[test]
    fn the_dip_matches_reference_values() {
        for (xs, want) in REFERENCE {
            let mut xs = xs.to_vec();
            xs.sort_by(f64::total_cmp);
            let got = dip(&xs);
            assert!((got - want).abs() < 1e-12, "{xs:?}: {got} against {want}");
        }
    }

    #[test]
    fn the_dip_of_small_and_constant_samples_follows_the_definition() {
        assert_eq!(dip(&[]), 0.0);
        assert_eq!(dip(&[5.0]), 0.0);
        assert_eq!(dip(&[1.0; 5]), 0.0);
        // Distinct values: a jump at one atom only, so the other needs half its step.
        assert_eq!(dip(&[0.0, 1.0]), 0.25);
        assert!((dip(&[0.0, 1.0, 2.0]) - 1.0 / 6.0).abs() < 1e-15);
        assert_eq!(dip(&[1.0, 1.0, 2.0, 2.0]), 0.25);
    }

    proptest::proptest! {
        /// The dip is unchanged by moving, scaling and mirroring the sample, and lies between
        /// 1/(2n) and 1/4 for any sample of two or more values (HH85 §3 (A)).
        #[test]
        fn the_dip_is_affine_invariant_and_bounded(
            raw in proptest::collection::vec(-1000i32..1000, 2..80),
            scale in 1i32..50,
            shift in -500i32..500,
        ) {
            let mut xs: Vec<f64> = raw.iter().map(|&v| f64::from(v)).collect();
            xs.sort_by(f64::total_cmp);
            let d = dip(&xs);
            let n = xs.len() as f64;
            if xs.first() != xs.last() {
                proptest::prop_assert!(d >= 1.0 / (2.0 * n) - 1e-12 && d <= 0.25 + 1e-12, "{d}");
            }
            let mut moved: Vec<f64> = xs.iter().map(|v| v * f64::from(scale) + f64::from(shift)).collect();
            moved.sort_by(f64::total_cmp);
            proptest::prop_assert!((dip(&moved) - d).abs() < 1e-9);
            let mut mirrored: Vec<f64> = xs.iter().map(|v| -v).collect();
            mirrored.sort_by(f64::total_cmp);
            proptest::prop_assert!((dip(&mirrored) - d).abs() < 1e-9);
        }
    }

    #[test]
    fn two_separated_states_are_found_and_split() {
        // A benchmark point whose rounds catch a stall about a third of the time. Independent
        // rounds fail one of the two order checks about a tenth of the time; this seed's pass.
        let mut rng = crate::measure::SplitMix64::new(0);
        let xs: Vec<f64> = (0..30)
            .map(|_| {
                let jitter = (rng.below(1000) as f64 - 500.0) / 500.0;
                if rng.below(3) == 0 {
                    1_600.0 + 200.0 * jitter
                } else {
                    15_400.0 + 100.0 * jitter
                }
            })
            .collect();
        let j = judge(&xs);
        let (d, c) = j.dip.unwrap();
        assert!(d > c, "dip {d} against {c}");
        assert_eq!(j.states.len(), 2);
        let (high, low) = (&j.states[0], &j.states[1]);
        assert!(high.median > 15_000.0 && low.median < 2_000.0);
        assert_eq!(high.rounds.len() + low.rounds.len(), 30);
        assert!(high.rounds.iter().all(|&i| xs[i] > 10_000.0));
        // Independent rounds, each state with six or more: both carry intervals.
        assert_eq!(j.order, Order::Independent);
        assert!(high.interval.is_some() && low.interval.is_some());
    }

    #[test]
    fn one_state_is_one_state() {
        // Bell-shaped rounds, each the sum of four uniform draws. (Uniform rounds are the dip
        // test's least favourable case and are judged two states 5% of the time by design.)
        let mut rng = crate::measure::SplitMix64::new(0);
        let xs: Vec<f64> = (0..30)
            .map(|_| 1000.0 + (0..4).map(|_| rng.below(250) as f64 / 100.0).sum::<f64>())
            .collect();
        let j = judge(&xs);
        assert_eq!(j.states.len(), 1);
        assert_eq!(j.order, Order::Independent);
        let s = &j.states[0];
        let (lo, hi) = s.interval.unwrap();
        assert!(lo <= s.median && s.median <= hi);
        assert!(s.spread().unwrap() < 0.01);
        assert!(Policy::STANDARD.enough(&j));
    }

    #[test]
    fn dependent_rounds_carry_no_interval() {
        // Alternating slow and fast rounds: a dip, two states, and no interval for either.
        let xs: Vec<f64> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    9_800.0 + i as f64
                } else {
                    1_700.0 + i as f64
                }
            })
            .collect();
        let j = judge(&xs);
        assert_eq!(j.order, Order::Alternating);
        assert_eq!(j.states.len(), 2);
        assert!(j.states.iter().all(|s| s.interval.is_none()));
        assert!(!Policy::STANDARD.enough(&j));
        // A slow stretch, then fast: states that persist.
        let xs: Vec<f64> = (0..20)
            .map(|i| {
                if (3..14).contains(&i) {
                    2_000.0 + i as f64
                } else {
                    15_000.0 + i as f64
                }
            })
            .collect();
        assert_eq!(judge(&xs).order, Order::Persistent);
    }

    #[test]
    fn a_point_stops_at_the_limit_or_once_settled() {
        let p = Policy::STANDARD;
        assert_eq!(p.limit(), 30);
        let steady: Vec<f64> = (0..9).map(|i| 100.0 + f64::from(i % 3) * 0.1).collect();
        assert!(!p.enough(&judge(&steady)), "below the minimum");
        let wild: Vec<f64> = (0..30)
            .map(|i| if i % 2 == 0 { 1.0 } else { 100.0 })
            .collect();
        assert!(p.enough(&judge(&wild)), "at the limit");
        let huge = Policy {
            min: 10,
            max: 10_000,
            precision: 0.05,
        };
        assert_eq!(huge.limit(), MAX_ROUNDS);
    }

    /// Samples and their dips; see `the_dip_matches_reference_values`.
    const REFERENCE: &[(&[f64], f64)] = &[
        (&[0.11912, 0.502516, 0.511823, 0.860001], 0.125),
        (
            &[0.568119, 0.42732, -1.026861, -0.756266, 0.262255],
            0.15801851811629053,
        ),
        (
            &[
                3.767712, 5.008567, -1.328993, 6.414038, 1.180859, 4.40252, -1.384944,
            ],
            0.14053028463101638,
        ),
        (
            &[
                0.251513, 0.979912, 0.929317, 0.805136, 0.999656, 0.513588, 0.07632, 0.340395,
                0.557451, 0.292122,
            ],
            0.11459005848298325,
        ),
        (
            &[
                1.113496, -1.093048, -0.231417, -0.92202, -1.008481, -0.60257, 0.001249, -1.354033,
                4.943346, 6.042175, -0.225285, -0.02557, 1.488713,
            ],
            0.08484718114652506,
        ),
        (
            &[
                0.472236, 0.352491, -0.303652, -0.810195, -0.783257, -0.716523, 0.080868, 1.058344,
                -0.922224, -1.084614, 0.301877, 2.667774, -1.266337, -1.228734, -0.36228,
                -0.414253, -0.750771, 0.36344, -1.123307, -2.059281,
            ],
            0.0725541895221991,
        ),
        (
            &[
                15427.5, 15396.9, 7093.6, 15425.8, 15174.9, 15414.8, 15213.4, 6711.0, 9364.6,
                15386.8, 2254.2, 8915.3, 15152.0, 9471.7, 15475.6, 15387.9, 15516.8, 5289.7,
                15402.1, 15400.2,
            ],
            0.0863941474098334,
        ),
        (
            &[
                2105.7, 3704.0, 15402.9, 7463.5, 15149.7, 15251.5, 6072.3, 2608.2, 15314.6,
                15597.3, 15347.0, 15285.5, 10175.7, 15196.1, 15443.2, 15286.8, 15564.3, 15529.4,
                15296.4, 15536.4, 15360.1, 15475.1, 15523.5, 2792.7, 15358.8, 15330.2, 1974.2,
                15515.6, 15426.8, 15385.6,
            ],
            0.0658281153150418,
        ),
        (
            &[
                3.399739, 0.499789, 5.378375, -0.365213, 2.509231, -0.264734, 4.189065, 0.671109,
                -0.310592, 0.864247, 0.195851, -1.018063, -0.502659, 5.056924, 5.783591, -1.775394,
                6.041511, -0.995168, 5.871889, 3.79188, 0.710456, 1.070671, 1.224527, 1.009051,
                0.185179, 0.814845, 0.628804, 5.465699, -0.674494, 3.45342,
            ],
            0.06612141539187176,
        ),
        (
            &[
                0.556072, 1.296054, -0.804349, -0.03351, 0.432876, -0.707474, -0.307003, 0.714339,
                0.950628, 0.396418, 1.388519, -1.03499, -0.151528, 0.090221, 0.177223, 0.122816,
                1.089316, 1.062549, -2.079828, -1.084221, -1.025195, 1.405354, -1.139542,
                -0.910069, -1.704819, -1.505759, -0.281068, 0.637782, -2.705413, 0.64646, 1.099739,
                -1.053685, 0.973597, -1.157484, 0.880892, -0.457713, 0.887225, -0.306485, -0.09824,
                -1.06695, 1.524697, 0.228379, -0.620046, 2.443214, 0.157373, -0.485973, 0.098322,
                -0.870847, 1.024748, -0.715785,
            ],
            0.044983392425364734,
        ),
        (
            &[
                -1.483834, -0.596817, 5.381621, 0.350696, 4.865134, -0.200611, -0.552046, 0.492233,
                0.883666, 5.885979, 4.593409, -0.919723, 0.796467, 0.117759, -0.950688, 2.206616,
                3.630139, 4.526351, -0.135223, 6.409446, -0.08288, 4.87319, 1.481455, 4.719265,
                5.838431, 4.438216, 5.206104, 1.868239, -0.90131, 1.102002, 0.316822, -1.279604,
                6.203741, 6.742362, 5.792988, 3.660285, 5.833828, 5.487535, -0.241493, -3.662168,
                0.768089, -1.075382, 3.120624, 1.751965, 5.651207, 0.99879, 4.609561, 0.673554,
                0.137959, -0.578744, -1.674722, 2.097858, 6.362352, 0.080976, 0.820397, 3.006229,
                5.574651, 1.059199, 1.075389, 1.791, 4.96079, -0.049517, 0.122029, 5.565787,
                1.146211, 4.891893, 3.983008, 5.940708, 0.427842, -0.273408, 4.552501, -1.00852,
                0.077838, -0.421758, 0.014808, -0.715731, -1.070739, 6.898974, 4.488458, 5.35679,
            ],
            0.07715708107649247,
        ),
        (
            &[
                15365.5, 5274.1, 5001.8, 15314.1, 7671.4, 15379.0, 15588.9, 5611.2, 15348.9,
                6744.6, 15347.7, 15302.2, 15426.4, 7838.9, 15302.5, 9519.4, 7776.0, 15195.7,
                15346.4, 8327.0, 15471.7, 15398.8, 15490.2, 15420.0, 4585.8, 15288.1, 15541.8,
                15534.5, 15428.0, 15506.9, 15535.0, 15473.1, 15473.8, 15450.6, 15351.9, 15362.2,
                15364.1, 15253.1, 15386.4, 15503.4, 10888.5, 10131.5, 4610.3, 15339.8, 9437.9,
                15352.3, 15580.2, 15424.8, 3227.7, 15455.4, 15315.4, 15369.0, 7713.2, 8728.7,
                15419.4, 15500.5, 15464.4, 15362.3, 15482.9, 9675.9, 15388.6, 6349.3, 15642.5,
                15346.2, 15355.9, 15277.9, 15393.8, 3925.6, 6039.3, 15447.9, 5102.4, 15325.2,
                15473.1, 8368.8, 15503.3, 7672.7, 15386.7, 10427.4, 15377.0, 15309.1, 15376.5,
                15460.5, 15421.2, 15274.6, 15408.1, 15317.8, 15494.3, 2803.7, 15393.7, 15372.0,
                15266.1, 3795.1, 2889.0, 15319.6, 5401.5, 15430.5, 15347.0, 3363.3, 15504.6,
                15394.6, 8138.9, 7404.7, 15529.4, 15356.1, 8468.8, 15539.4, 15475.3, 15467.3,
                15526.4, 10835.5, 15542.2, 8957.7, 15373.8, 15521.1, 9452.5, 15407.3, 15434.4,
                5723.4, 15276.6, 15535.0,
            ],
            0.05358510867226167,
        ),
        (&[-1.0, 0.0, 2.0, 1.0, 3.0, 0.0], 0.08333333333333333),
        (
            &[4.0, 1.0, -1.0, -1.0, 3.0, 4.0, 4.0, -1.0, 0.0],
            0.16666666666666666,
        ),
        (
            &[
                5.0, 1.0, 1.0, -1.0, -1.0, 2.0, -2.0, 0.0, -1.0, -1.0, 3.0, -1.0,
            ],
            0.08333333333333333,
        ),
        (
            &[
                0.0, 4.0, 4.0, 1.0, 0.0, 0.0, 0.0, 4.0, 1.0, 4.0, 2.0, 6.0, 0.0, 1.0, 4.0, 0.0,
            ],
            0.15625,
        ),
        (
            &[
                0.0, 2.0, 5.0, -1.0, 2.0, 0.0, 2.0, -1.0, 2.0, 2.0, 4.0, -1.0, 6.0, 1.0, -1.0, 0.0,
                0.0, 0.0, 3.0, 0.0, 5.0, 5.0, -1.0, 4.0, 4.0,
            ],
            0.1,
        ),
    ];
}
