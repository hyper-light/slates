//! Which shard replies the WinFsp receive worker spins for, chosen kind by kind (audit step 7, cut (b); amends A-212).
//!
//! A-212's worker spins for every reply, for the window it measured as it started (the median one-way wake through its
//! reply channel), and then parks: the 2-competitive rule the shards use (§4.3) [A: Karlin, Manasse, McGeoch & Owicki
//! 1994]. One window for every kind of request spins where spinning cannot pay. On CI's arm64 runner, quiet, a Create's
//! reply came within the window for 0.2 % of its waits and a Rename's for 0.1 %, each such wait spinning 9.4 µs of CPU
//! before it slept, while an Info's came within it for 98.2 %; loaded, a Create's came within it for 19.3 % and a
//! SetSize's for 6.5 %; on the x86_64 runner, loaded, a Create's for 3.9 %, a Rename's for 2.4 % and a directory
//! listing's for 1.0 % (diagnostics run 42506936806, `diag-winfsp-overlap-50`, five runs a pass).
//!
//! So each kind of request chooses for itself, by the same model's costs over its own recent replies, between spinning
//! the window and parking at once. A wait that spins costs the time it spins; a park costs one wake, which is the
//! window. A reply that came `T` after its request was posted cost a spinning wait `T` when `T` was within the window,
//! and twice the window when it was not (spun out, then woken); parking at once would have cost the window either way.
//! A kind spins while the weighted mean of what spinning cost its replies is at most the window: exactly when, by its
//! recent replies, spinning is expected to cost no more than parking. A kind whose replies mostly come within the window
//! spins as A-212 does; one whose replies mostly come after it parks at once and leaves the CPU to the shards.
//!
//! Every reply carries the instant the shard sent it (`crate::host::Served::send`), so `T` is known however a wait
//! ended. A kind that parks keeps learning from each reply, and spins again once its replies come within the window:
//! no wait is spent probing.
//!
//! The mean is exponentially weighted over about [`WEIGHED`] replies, the precision an online estimate of the wake asks
//! of itself (`slates_machine::wake::WakeLatency::estimate_window`), and kept in fixed point as that estimate is
//! (`slates_machine::wake::WakeEstimate`), so a small correction is never rounded away. A kind with no reply yet stands
//! at the window, where the two choices cost the same; ties spin, as A-212 did for every kind. The choice is pure and
//! tested on every host; the Windows worker (`crate::reply`) asks it before each wait and tells it each reply.

use std::time::Duration;

/// The kind of shard request a reply answers: one per `crate::host::Op` variant, whose `reply_kind` maps each
/// exhaustively, so a new request kind is a compile error until it is given its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyKind {
  /// A kernel-name update after a successful rename.
  RenameName,
  /// The confirmation that the driver accepted an open (A-274).
  Confirm,
  /// The volume's size and label.
  VolumeInfo,
  /// A path resolved to its object and attributes (`GetSecurityByName`).
  Resolve,
  /// An existing file or directory opened.
  Open,
  /// A coherent purge's resolve (A-190).
  PurgeResolve,
  /// A coherent purge's open (A-190).
  PurgeOpen,
  /// A file or directory made.
  Create,
  /// A read.
  Read,
  /// A write.
  Write,
  /// An open's attributes.
  Info,
  /// A directory's listing.
  ReadDir,
  /// A file's size set.
  SetSize,
  /// An open's overwrite.
  Truncate,
  /// A rename.
  Rename,
  /// A file's allocation set.
  SetAllocation,
  /// An object's times or attributes set.
  SetTimes,
  /// Whether an object may be deleted.
  CanDelete,
  /// An open's data flushed.
  Flush,
  /// The volume flushed.
  FlushVolume,
  /// An open's cleanup.
  Cleanup,
  /// An open's close.
  Release,
  /// A link resolved by its name (A-275).
  LinkByName,
  /// An open link's target (A-275).
  LinkOf,
  /// A symbolic link made (A-275).
  MakeLink,
  /// A symbolic link removed (A-275).
  UnmakeLink,
}

impl ReplyKind {
  /// Every kind, in [`ReplyKind::index`] order.
  pub const ALL: [ReplyKind; 26] = [
    ReplyKind::RenameName,
    ReplyKind::Confirm,
    ReplyKind::VolumeInfo,
    ReplyKind::Resolve,
    ReplyKind::Open,
    ReplyKind::PurgeResolve,
    ReplyKind::PurgeOpen,
    ReplyKind::Create,
    ReplyKind::Read,
    ReplyKind::Write,
    ReplyKind::Info,
    ReplyKind::ReadDir,
    ReplyKind::SetSize,
    ReplyKind::Truncate,
    ReplyKind::Rename,
    ReplyKind::SetAllocation,
    ReplyKind::SetTimes,
    ReplyKind::CanDelete,
    ReplyKind::Flush,
    ReplyKind::FlushVolume,
    ReplyKind::Cleanup,
    ReplyKind::Release,
    ReplyKind::LinkByName,
    ReplyKind::LinkOf,
    ReplyKind::MakeLink,
    ReplyKind::UnmakeLink,
  ];

  /// This kind's place in [`ReplyKind::ALL`].
  pub fn index(self) -> usize {
    // A field-less enum's discriminant is its declaration order, which `ALL` lists.
    self as usize
  }

  /// The kind's name, for the worker's closing report.
  pub fn name(self) -> &'static str {
    match self {
      ReplyKind::RenameName => "RenameName",
      ReplyKind::Confirm => "Confirm",
      ReplyKind::VolumeInfo => "VolumeInfo",
      ReplyKind::Resolve => "Resolve",
      ReplyKind::Open => "Open",
      ReplyKind::PurgeResolve => "PurgeResolve",
      ReplyKind::PurgeOpen => "PurgeOpen",
      ReplyKind::Create => "Create",
      ReplyKind::Read => "Read",
      ReplyKind::Write => "Write",
      ReplyKind::Info => "Info",
      ReplyKind::ReadDir => "ReadDir",
      ReplyKind::SetSize => "SetSize",
      ReplyKind::Truncate => "Truncate",
      ReplyKind::Rename => "Rename",
      ReplyKind::SetAllocation => "SetAllocation",
      ReplyKind::SetTimes => "SetTimes",
      ReplyKind::CanDelete => "CanDelete",
      ReplyKind::Flush => "Flush",
      ReplyKind::FlushVolume => "FlushVolume",
      ReplyKind::Cleanup => "Cleanup",
      ReplyKind::Release => "Release",
      ReplyKind::LinkByName => "LinkByName",
      ReplyKind::LinkOf => "LinkOf",
      ReplyKind::MakeLink => "MakeLink",
      ReplyKind::UnmakeLink => "UnmakeLink",
    }
  }
}

/// Format: the kinds of request, one state each.
const KINDS: usize = ReplyKind::ALL.len();

/// Format: the standard normal quantile of the two-sided 95 % interval, in thousandths (1.960), as the machine
/// profile's wake probe takes it (`slates_machine::wake`).
const Z_95_PERMILLE: u64 = 1960;

/// Derived: the replies a kind's mean spinning cost weighs: `N = ⌈z · σ / (h · W)⌉²`, as an online estimate of the wake
/// sizes itself (`slates_machine::wake::WakeLatency::estimate_window`), with `z` the 95 % normal quantile, `h` half the
/// probe's stopping-rule width (`slates_machine::stats::CONVERGED_WIDTH_PERMILLE`) and `σ` at most the window `W`,
/// since a spinning cost lies between none and twice the window: `⌈1.96 / 0.05⌉² = 40² = 1,600` replies, so a kind's
/// mean is within ±5 % of the window at 95 %.
pub const WEIGHED: u64 = weighed();

/// Derived: the exponential weighting's shift, `⌈log₂ WEIGHED⌉` (11), as the wake estimate takes its own
/// (`slates_machine::wake::WakeLatency::estimate_shift`).
const SHIFT: u32 = shift_of(WEIGHED);

/// Derived: the fixed point's unit, `2^SHIFT`: a mean is kept multiplied by it.
const SCALE: u128 = 2u128.saturating_pow(SHIFT);

const fn weighed() -> u64 {
  match slates_machine::stats::CONVERGED_WIDTH_PERMILLE.checked_div(2) {
    Some(half_width) if half_width > 0 => {
      let ratio = Z_95_PERMILLE.div_ceil(half_width);
      ratio.saturating_mul(ratio)
    }
    // A stopping rule with no width asks for no end of replies: weigh as many as can be counted.
    _ => u64::MAX,
  }
}

const fn shift_of(replies: u64) -> u32 {
  match replies.saturating_sub(1).checked_ilog2() {
    Some(bits) => bits.saturating_add(1),
    None => 0,
  }
}

/// One kind's standing: its weighted mean spinning cost, and what its choice did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Standing {
  /// The weighted mean spinning cost in nanoseconds, multiplied by [`SCALE`]; `None` before the kind's first reply.
  scaled: Option<u128>,
  /// Waits that spun the window.
  spun: u64,
  /// Waits the kind's choice parked at once (the non-vacuity count: a choice that never parks is A-212 unchanged).
  parked_at_once: u64,
}

/// What a kind's choice did over a worker's life, for its closing report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
  /// Waits that spun the window.
  pub spun: u64,
  /// Waits the kind's choice parked at once.
  pub parked_at_once: u64,
  /// The kind's weighted mean spinning cost, nanoseconds; `None` before its first reply.
  pub mean_cost_ns: Option<u64>,
}

/// One worker's choices, a kind at a time.
#[derive(Clone, Debug)]
pub struct SpinChoices {
  kinds: [Standing; KINDS],
}

impl Default for SpinChoices {
  fn default() -> Self {
    Self::new()
  }
}

impl SpinChoices {
  /// Every kind standing at the window: spinning, as A-212 does, until its replies say otherwise.
  pub const fn new() -> Self {
    Self {
      kinds: [Standing {
        scaled: None,
        spun: 0,
        parked_at_once: 0,
      }; KINDS],
    }
  }

  /// How long a wait for a `kind` reply spins before it parks: the window, or none when the kind's replies say parking at
  /// once costs less. A zero window spins for none whatever the kind, and is counted as neither choice.
  pub fn spin_for(&mut self, kind: ReplyKind, window: Duration) -> Duration {
    let Some(standing) = self.kinds.get_mut(kind.index()) else {
      return window;
    };
    if window.is_zero() {
      return Duration::ZERO;
    }
    let threshold = window.as_nanos().saturating_mul(SCALE);
    if standing.scaled.is_none_or(|scaled| scaled <= threshold) {
      standing.spun = standing.spun.saturating_add(1);
      window
    } else {
      standing.parked_at_once = standing.parked_at_once.saturating_add(1);
      Duration::ZERO
    }
  }

  /// Folds in a `kind` reply that came `came_after` its request was posted, against `window`: what a spinning wait cost
  /// it (`came_after` within the window, twice the window past it), whichever way the wait went.
  pub fn record(&mut self, kind: ReplyKind, came_after: Duration, window: Duration) {
    let Some(standing) = self.kinds.get_mut(kind.index()) else {
      return;
    };
    let window_ns = window.as_nanos();
    let cost = if came_after <= window {
      came_after.as_nanos()
    } else {
      window_ns.saturating_mul(2)
    };
    let scaled = standing
      .scaled
      .unwrap_or_else(|| window_ns.saturating_mul(SCALE));
    standing.scaled = Some(
      scaled
        .saturating_sub(scaled.checked_div(SCALE).unwrap_or(0))
        .saturating_add(cost),
    );
  }

  /// What `kind`'s choice did, and where its mean stands.
  pub fn counts(&self, kind: ReplyKind) -> Counts {
    self
      .kinds
      .get(kind.index())
      .map(|standing| Counts {
        spun: standing.spun,
        parked_at_once: standing.parked_at_once,
        mean_cost_ns: standing
          .scaled
          .map(|scaled| u64::try_from(scaled.checked_div(SCALE).unwrap_or(0)).unwrap_or(u64::MAX)),
      })
      .unwrap_or_default()
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
  use super::*;

  /// Shape: a window like the ones the runners measure (5.0 to 16.1 µs, run 43148071819).
  const WINDOW: Duration = Duration::from_micros(7);

  /// Shape: replies a test feeds, as a multiple of [`WEIGHED`]: enough for any seed to have decayed.
  const SETTLED: u64 = 4;

  /// Feeds `kind` replies at `came_after`, each wait asking first, and answers how many spun.
  fn waits(choices: &mut SpinChoices, kind: ReplyKind, came_after: Duration, count: u64) -> u64 {
    let mut spun = 0;
    for _ in 0..count {
      if choices.spin_for(kind, WINDOW) == WINDOW {
        spun += 1;
      }
      choices.record(kind, came_after, WINDOW);
    }
    spun
  }

  /// Cut (b). Do: wait on a kind whose replies all come within the window. Expect: every wait spins and none parks at
  /// once, as A-212 spins: a kind whose spinning pays is never taken off it.
  #[test]
  fn a_kind_whose_replies_come_within_the_window_always_spins() {
    let mut choices = SpinChoices::new();
    let count = WEIGHED * SETTLED;
    assert_eq!(
      waits(&mut choices, ReplyKind::Info, WINDOW / 4, count),
      count
    );
    assert_eq!(choices.counts(ReplyKind::Info).parked_at_once, 0);
  }

  /// Cut (b). Do: wait on a kind whose replies all come after the window. Expect: only the first wait spins (a kind with
  /// no reply stands at the window, where the choices cost the same); every later one parks at once, and the count says
  /// so: the CPU a spin would waste goes to the shards.
  #[test]
  fn a_kind_whose_replies_come_after_the_window_parks_at_once_from_its_second_wait() {
    let mut choices = SpinChoices::new();
    assert_eq!(
      waits(&mut choices, ReplyKind::Create, WINDOW * 5, WEIGHED),
      1
    );
    assert_eq!(
      choices.counts(ReplyKind::Create).parked_at_once,
      WEIGHED - 1
    );
  }

  /// Cut (b). Do: for each kind, make its replies slow on a fresh worker. Expect: that kind parks at once, and every
  /// other kind still spins its first wait and has no standing: one kind's replies never move another's.
  #[test]
  fn each_kind_chooses_by_its_own_replies() {
    for slow in ReplyKind::ALL {
      let mut choices = SpinChoices::new();
      waits(&mut choices, slow, WINDOW * 5, WEIGHED);
      assert_eq!(
        choices.spin_for(slow, WINDOW),
        Duration::ZERO,
        "{}",
        slow.name()
      );
      for other in ReplyKind::ALL.into_iter().filter(|kind| *kind != slow) {
        assert_eq!(
          choices.counts(other),
          Counts::default(),
          "{}'s replies moved {}",
          slow.name(),
          other.name()
        );
        assert_eq!(choices.spin_for(other, WINDOW), WINDOW, "{}", other.name());
      }
    }
  }

  /// Cut (b). Do: let a kind park at once on slow replies, then make its replies fast. Expect: it spins again within the
  /// replies its mean weighs, learning from replies it parked for: no wait is spent probing, and no kind parks for good.
  #[test]
  fn a_kind_that_parks_spins_again_once_its_replies_come_within_the_window() {
    let mut choices = SpinChoices::new();
    waits(&mut choices, ReplyKind::Read, WINDOW * 5, WEIGHED * SETTLED);
    let mut parked = 0;
    while choices.spin_for(ReplyKind::Read, WINDOW) == Duration::ZERO {
      choices.record(ReplyKind::Read, Duration::ZERO, WINDOW);
      parked += 1;
      assert!(
        parked <= WEIGHED,
        "still parking after {parked} replies within the window"
      );
    }
    assert!(
      parked > 0,
      "the kind never parked, so its return proves nothing"
    );
  }

  /// A fixed pseudo-random sequence (xorshift64), so the mixture test replays the same replies on every host.
  struct Replies(u64);

  impl Replies {
    fn next(&mut self) -> u64 {
      self.0 ^= self.0 << 13;
      self.0 ^= self.0 >> 7;
      self.0 ^= self.0 << 17;
      self.0
    }

    /// A reply within the window with probability `permille_within` thousandths, else past it.
    fn reply(&mut self, permille_within: u64) -> Duration {
      let window_ns = u64::try_from(WINDOW.as_nanos()).unwrap();
      if self.next() % 1000 < permille_within {
        Duration::from_nanos(self.next() % window_ns)
      } else {
        WINDOW + Duration::from_nanos(1 + self.next() % (window_ns * 10))
      }
    }
  }

  /// The rule in floating point: what a spinning wait cost a reply that came `came_after`.
  fn spinning_cost(came_after: Duration) -> f64 {
    if came_after <= WINDOW {
      came_after.as_nanos() as f64
    } else {
      2.0 * WINDOW.as_nanos() as f64
    }
  }

  /// Feeds one kind replies `permille_within` thousandths within the window, comparing each choice with the rule's
  /// reference mean; answers how many choices were compared.
  fn compare_with_the_rule(permille_within: u64) -> u64 {
    let window_ns = WINDOW.as_nanos() as f64;
    let weight = 1.0 / SCALE as f64;
    let mut replies = Replies(0x9E37_79B9_7F4A_7C15 ^ permille_within);
    let mut choices = SpinChoices::new();
    let mut reference = window_ns;
    let mut compared = 0;
    for _ in 0..WEIGHED * SETTLED * 2 {
      let spins = choices.spin_for(ReplyKind::Write, WINDOW) == WINDOW;
      let clear_of_the_threshold = (reference - window_ns).abs() > window_ns / 100.0;
      assert!(
        !clear_of_the_threshold || spins == (reference <= window_ns),
        "{permille_within} permille within: the rule's mean {reference:.1} ns against the window {window_ns} ns"
      );
      compared += u64::from(clear_of_the_threshold);
      let came_after = replies.reply(permille_within);
      choices.record(ReplyKind::Write, came_after, WINDOW);
      reference += (spinning_cost(came_after) - reference) * weight;
    }
    compared
  }

  /// Cut (b), the 2-competitive model per kind. Do: feed kinds mixtures of replies within and past the window, in a fixed
  /// pseudo-random order, and compare each choice with the rule stated in floating point over the same weighting: spin
  /// while the weighted mean of `T` (within the window) or twice the window (past it) is at most the window. Expect: the
  /// same choice wherever the rule's mean is more than a hundredth of the window from it, where fixed and floating point
  /// may round apart; and a mixture on each side of the threshold among them.
  #[test]
  fn the_choice_is_the_competitive_rule_over_each_kinds_replies() {
    for permille_within in [0, 200, 450, 500, 550, 800, 1000] {
      assert!(
        compare_with_the_rule(permille_within) > 0,
        "{permille_within} permille: nothing compared"
      );
    }
  }

  /// Cut (b). Do: ask with a zero window, as a thread that measured none waits. Expect: no spin, whatever the kind's
  /// replies, and neither choice counted.
  #[test]
  fn a_zero_window_spins_for_none() {
    let mut choices = SpinChoices::new();
    assert_eq!(
      choices.spin_for(ReplyKind::Info, Duration::ZERO),
      Duration::ZERO
    );
    choices.record(ReplyKind::Info, Duration::ZERO, Duration::ZERO);
    assert_eq!(
      choices.spin_for(ReplyKind::Info, Duration::ZERO),
      Duration::ZERO
    );
    let counts = choices.counts(ReplyKind::Info);
    assert_eq!((counts.spun, counts.parked_at_once), (0, 0));
  }
}
