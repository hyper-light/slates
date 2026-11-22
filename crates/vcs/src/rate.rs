//! A repository's learned demand: its decayed rates of works and bytes and the bytes a work brings, whose decay is
//! learned from the repository's own works (A-724 piece 2; §4.17.5 "Learned demand"; §4.17.1 "Every tunable is learned online").
//!
//! **What it is.** §4.17.5's shares weigh each repository by what it actually does now: how many works arrive a second
//! and how many bytes they bring. A count since the start weighs last month like the last minute, and a window or a
//! decay set by hand is a literal (R3). An exponentially decayed count with time constant `τ`, `v(t) = Σ x_i e^{-(t -
//! t_i)/τ}`, read as the rate `v / τ`, forgets at `τ`'s pace; the question is which `τ`. A toy repository pushed twice
//! a day and a monorepo taking a push a second have horizons six orders apart, so `τ` must be the repository's own.
//!
//! **The horizon, learned.** A ladder of estimators at `τ_k = base · 2^k` (`base` the caller's clock grain) keeps one
//! decayed count each, a rung added as the repository's age passes its `τ`, so at most 64 rungs. At each arrival every
//! rung first predicts the rate from the arrivals before it, and the gap just seen is scored by its log-likelihood under
//! a Poisson process at that rate, the intensity at the arrival less its integral over the gap, the proper score for a point process [A: Daley and Vere-Jones, "An
//! Introduction to the Theory of Point Processes", 2003, §7.1]. Each rung's scores are summed, decayed at the slowest
//! rung's pace so an old regime fades too, and the rung with the best sum is the horizon: the argmin of a cost model
//! (§4.17.1's estimator kinds), chosen by the repository's own gaps. Too short a `τ` forgets between arrivals and
//! predicts a rate near zero, too long a one still predicts last week's; the likelihood weighs both. Stretching time by
//! two moves the choice up exactly one rung, so the estimate is scale-free. Measured and rejected (`rate::tests`, 2026-11-21): scoring
//! a gap by the intensity at its end times the gap, rather than the intensity's integral over it, under-charged short
//! rungs, whose intensity has fallen by then; on 3,000 Poisson arrivals at 10 a second it chose a horizon of 1.3 gaps
//! and read 18.8 a second. With the integral it chose 84 gaps and read 9.9.
//!
//! **Bytes a work** (§4.17.5's demand: what the next work will claim; it replaces A-724 piece 2a's `Learned`, so a
//! repository has one learned demand). Each rung keeps the decayed mean of its works' bytes beside their count, and the
//! same ladder chooses a second horizon for it: the rung whose mean would have predicted each work's bytes best, by
//! squared error (the proper score for a mean), its sums decayed as the gaps' are. The arrival horizon cannot serve:
//! a steady rhythm makes the longest rung the most likely, and 100 small intakes then three 64 times larger at that
//! rhythm would read about 2.8 small where the shard must expect the large. Among equal scores, which only an
//! unchanging size gives, the shortest rung wins (equal evidence goes to the most recent past): so a new size is read
//! from the first work that brings it, and a work after a silence outweighs what came before it. A mean is a convex
//! combination of the bytes seen, so the estimate never leaves their range; a first work's bytes are the first value.
//! Bytes a second are the arrival horizon's works a second times its mean. The miss-ratio curve's aging is the
//! arrival horizon's too: [`Rates::keep`] is `e^{-elapsed/τ}`, what `crate::mrc::Curve::decay` takes.
//!
//! **A principal's rates** ([`Decayed`]) are one decayed count each, read at its repository's horizon: a repository
//! keeps its principals in a Space-Saving summary of bounded slots (`crate::usage`), and a ladder a slot would cost each
//! of them its rungs.
//!
//! **Before any gap** there is no rate to read: [`Rates::works_per_s`] answers `None` until a second work, and the
//! caller takes its first value from the work in hand (§4.17.1: that work's size at the rate this machine was measured
//! to do it), never a literal. Arithmetic is in `f64`, where nothing panics; times are the caller's clock in
//! nanoseconds.

/// Format: nanoseconds in a second, the unit rates are read in.
const NANOS_PER_SECOND: f64 = 1e9;

/// Format: the most rungs: one for each power of two a `u64` nanosecond count can reach.
const MOST_RUNGS: usize = 64;

/// One rung of the ladder (module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rung {
  /// Its time constant, in nanoseconds.
  tau_ns: f64,
  /// Its decayed count of works, as of the last work.
  works: f64,
  /// Its decayed mean bytes a work: the works' bytes weighted as their counts are, kept as a mean so a steady size is
  /// held exactly.
  mean: f64,
  /// Its summed log-likelihood of the gaps it predicted, decayed at the slowest rung's pace.
  score: f64,
  /// Its summed squared error predicting each work's bytes by its mean, negated (higher is better), decayed alike.
  size_score: f64,
}

/// A repository's learned demand of one kind of work (module docs).
#[derive(Clone, Debug, PartialEq)]
pub struct Rates {
  /// The clock grain the ladder starts at, in nanoseconds (at least one).
  base_ns: u64,
  /// The first work's time.
  first_ns: Option<u64>,
  /// The last work's time: what the counts are kept as of, and where the next gap is measured from.
  last_ns: u64,
  /// The rungs, shortest first.
  rungs: Vec<Rung>,
  /// The rung chosen as the horizon of the rates.
  chosen: usize,
  /// The rung chosen as the horizon of the bytes a work.
  sized: usize,
  /// Works seen.
  works: u64,
}

impl Rates {
  /// No work yet, the ladder starting at `base_ns` (the caller's clock grain; at least one).
  pub fn new(base_ns: u64) -> Rates {
    Rates {
      base_ns: base_ns.max(1),
      first_ns: None,
      last_ns: 0,
      rungs: Vec::new(),
      chosen: 0,
      sized: 0,
      works: 0,
    }
  }

  /// One work answered at `now_ns`, having brought `bytes` (module docs): each rung scores the gap since the last work
  /// and its prediction of these bytes, both horizons are chosen again, and each rung counts it.
  pub fn answered(&mut self, now_ns: u64, bytes: u64) {
    let sample = crate::share::amount_of(bytes);
    self.works = self.works.saturating_add(1);
    let Some(first) = self.first_ns else {
      self.first_ns = Some(now_ns);
      self.last_ns = now_ns;
      self.grow(0);
      for rung in &mut self.rungs {
        rung.works = 1.0;
        rung.mean = sample;
      }
      return;
    };
    let gap = crate::share::amount_of(now_ns.saturating_sub(self.last_ns));
    self.grow(now_ns.saturating_sub(first).max(1));
    let slowest = self.rungs.last().map_or(0.0, |rung| rung.tau_ns);
    let fade = (-gap / slowest).exp();
    for rung in &mut self.rungs {
      // The gap's log-likelihood under the intensity the works before it predict, `c e^{-s/τ} / τ` from the last
      // work, `c` the count then: the intensity at this work, less its integral over the gap, `c (1 - e^{-gap/τ})`.
      let decay = (-gap / rung.tau_ns).exp();
      let intensity = (rung.works * decay / rung.tau_ns).max(f64::MIN_POSITIVE);
      let compensator = rung.works * (1.0 - decay);
      rung.score = rung.score * fade + (intensity.ln() - compensator);
      let miss = sample - rung.mean;
      rung.size_score = rung.size_score * fade - miss * miss;
      let kept = rung.works * decay;
      rung.works = kept + 1.0;
      rung.mean += miss / rung.works;
    }
    self.last_ns = self.last_ns.max(now_ns);
    self.chosen = best(self.rungs.iter().map(|rung| rung.score));
    self.sized = best(self.rungs.iter().map(|rung| rung.size_score));
  }

  /// Works seen.
  pub fn works(&self) -> u64 {
    self.works
  }

  /// The bytes the next work is expected to bring (module docs, "Bytes a work"): `None` before any work, the first
  /// work's own bytes until a second, then the mean at the size horizon. Always within the range of the bytes seen.
  pub fn bytes_per_work(&self) -> Option<u64> {
    if self.works == 0 {
      return None;
    }
    self
      .rungs
      .get(self.sized)
      .map(|rung| crate::share::whole_units(rung.mean.round()))
  }

  /// The horizon chosen for the rates, in nanoseconds. `None` before a gap was scored.
  pub fn horizon_ns(&self) -> Option<f64> {
    self.scored()?.get(self.chosen).map(|rung| rung.tau_ns)
  }

  /// Works a second at `now_ns`, at the horizon. `None` before a gap was scored (module docs, "Before any gap").
  pub fn works_per_s(&self, now_ns: u64) -> Option<f64> {
    let rung = self.scored()?.get(self.chosen)?;
    Some(self.at(rung.works, rung.tau_ns, now_ns))
  }

  /// Bytes a second at `now_ns`, at the horizon: the works a second times their mean there. `None` before a gap.
  pub fn bytes_per_s(&self, now_ns: u64) -> Option<f64> {
    let rung = self.scored()?.get(self.chosen)?;
    Some(self.at(rung.works * rung.mean, rung.tau_ns, now_ns))
  }

  /// What a count kept since `elapsed_ns` ago keeps at the horizon, `e^{-elapsed/τ}` in `[0, 1]`: the miss-ratio
  /// curve's aging (`crate::mrc::Curve::decay`). One (keep everything) before a gap was scored.
  pub fn keep(&self, elapsed_ns: u64) -> f64 {
    self.horizon_ns().map_or(1.0, |tau| {
      (-crate::share::amount_of(elapsed_ns) / tau)
        .exp()
        .clamp(0.0, 1.0)
    })
  }

  /// The rungs, once a gap was scored.
  fn scored(&self) -> Option<&[Rung]> {
    (self.works > 1).then_some(self.rungs.as_slice())
  }

  /// `count`, kept as of the last work on a rung of time constant `tau_ns`, read as a rate a second at `now_ns`.
  fn at(&self, count: f64, tau_ns: f64, now_ns: u64) -> f64 {
    let since = crate::share::amount_of(now_ns.saturating_sub(self.last_ns));
    count * (-since / tau_ns).exp() / tau_ns * NANOS_PER_SECOND
  }

  /// Adds the rungs whose time constant the repository's `age_ns` has reached, and the one after them, each a copy of
  /// the longest before it (the closest count the ladder has of the history it did not see), up to [`MOST_RUNGS`].
  fn grow(&mut self, age_ns: u64) {
    while self.rungs.len() < MOST_RUNGS {
      let shift = u32::try_from(self.rungs.len()).unwrap_or(u32::MAX);
      let Some(tau) = 1u64
        .checked_shl(shift)
        .and_then(|power| self.base_ns.checked_mul(power))
      else {
        return;
      };
      if let Some(last) = self.rungs.last()
        && last.tau_ns > crate::share::amount_of(age_ns)
      {
        return;
      }
      let template = self.rungs.last().copied().unwrap_or(Rung {
        tau_ns: 0.0,
        works: 0.0,
        mean: 0.0,
        score: 0.0,
        size_score: 0.0,
      });
      self.rungs.push(Rung {
        tau_ns: crate::share::amount_of(tau),
        ..template
      });
    }
  }
}

/// The index of the best score, the first (shortest rung) among equals: equal evidence goes to the most recent past.
fn best(scores: impl Iterator<Item = f64>) -> usize {
  let mut chosen: Option<(usize, f64)> = None;
  for (at, score) in scores.enumerate() {
    if chosen.is_none_or(|(_, held)| score > held) {
      chosen = Some((at, score));
    }
  }
  chosen.map_or(0, |(at, _)| at)
}

/// A principal's decayed counts of works and bytes, read at its repository's horizon (module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Decayed {
  /// Its decayed count of works, as of `last_ns`.
  works: f64,
  /// Its decayed count of bytes, as of `last_ns`.
  bytes: f64,
  /// Its last event's time.
  last_ns: u64,
}

impl Decayed {
  /// Counts `works` and `bytes` at `now_ns`, the counts before decayed at `tau_ns` (its repository's horizon; a
  /// horizon not yet learned keeps everything).
  pub fn add(&mut self, now_ns: u64, (works, bytes): (u64, u64), tau_ns: Option<f64>) {
    let decay = Self::decay(now_ns.saturating_sub(self.last_ns), tau_ns);
    self.works = self.works * decay + works as f64;
    self.bytes = self.bytes * decay + bytes as f64;
    self.last_ns = self.last_ns.max(now_ns);
  }

  /// Works and bytes a second at `now_ns`, read at `tau_ns`; `None` without a horizon.
  pub fn per_s(&self, now_ns: u64, tau_ns: Option<f64>) -> Option<(f64, f64)> {
    let tau = tau_ns.filter(|tau| *tau > 0.0)?;
    let decay = Self::decay(now_ns.saturating_sub(self.last_ns), Some(tau));
    let scale = decay / tau * NANOS_PER_SECOND;
    Some((self.works * scale, self.bytes * scale))
  }

  /// `e^{-elapsed/τ}`, one without a horizon.
  fn decay(elapsed_ns: u64, tau_ns: Option<f64>) -> f64 {
    match tau_ns {
      Some(tau) if tau > 0.0 => (-(elapsed_ns as f64) / tau).exp(),
      _ => 1.0,
    }
  }
}

#[cfg(test)]
#[path = "rate/tests.rs"]
mod tests;
