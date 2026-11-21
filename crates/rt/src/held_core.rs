//! Whether a shard keeps its core when it harvests its driver, and for how long an idle spin may (§4.3 "Loop"; A-218;
//! 85a item 3, main's ruling (c), 2026-11-21).
//!
//! A shard harvests its driver between busy steps once a quantum has passed since its last wait, and on every turn of
//! its idle spin. On Linux and Windows a zero-timeout poll returns at once. On macOS a zero-timeout `kevent` sleeps
//! 14.6-15.5 µs with an involuntary switch (A-218), and the quantum is the wake estimate, a few microseconds, so every
//! busy step after the first paid that sleep: a live write's exchange hands work between two of the control shard's
//! own tasks twice, and paid about 30 µs for it (`fleet.live.hop.exchange.{spawn,wake}` 15.4 µs each at the median,
//! adas-mac-studio-2, 2026-11-21) against Linux's 6.
//!
//! The sleep is not all waste: it hands the core to a thread queued for it, and A-218 kept it, busy steps included,
//! because a true spinner on macOS held a core a woken client waited for and lost every tail once the host had no idle
//! core. So the shard keeps its core (`kevent64` with `KEVENT_FLAG_IMMEDIATE`, `crate::driver::Driver::poll_now`) only
//! while a core is free for it, and sleeps as before otherwise.
//!
//! - **Is a core free?** The kernel's own count of processors no runnable thread wanted, `HOST_LOAD_INFO`'s first Mach
//!   factor (`slates_machine::facts::free_processors`), read on a cold path: at most once per [`REFRESH_NS`], the
//!   interval the kernel recomputes it in, since a read between two recomputations learns nothing. macOS's per-CPU idle
//!   ticks cannot answer it (A-218: 0.0 idle cores with no extra load, 5.8 beside 36 spinners), nor can the shard's own
//!   turns (a spinner on macOS is rarely preempted for the thread it delays) or its wake estimate (a shard that keeps
//!   its core parks less and learns fewer wakes). The Mach factor is a one-minute average, so it trails a load that has
//!   just begun; that lag is the rule's known weakness, and the A/B's loaded arms measure it.
//! - **For how long may an idle spin keep its core?** At most what blocking costs: the shard's measured park and wake
//!   ([`spin_keeps_core`], the wake estimate). Spinning up to the cost of blocking and then blocking is within twice the
//!   optimal waiting cost whatever the wait turns out to be [A: Karlin, Li, Manasse and Owicki, "Empirical studies of
//!   competitive spinning for a shared-memory multiprocessor", SOSP 1991]. Past that bound an idle spin's turns sleep as
//!   before, and the window still ends in a park.
//! - **Counted** (`crate::shard::Counters`): harvests that kept the core and those that slept, the kept spins that
//!   caught work and those that ran out of their bound, and the nanoseconds spent spinning with the core kept: the CPU
//!   the rule costs, weighed against the wait it saves.

/// Derived: how often a shard asks the kernel whether a core is free: the interval the scheduler recomputes its load
/// figures in, the Mach factor among them (XNU `osfmk/kern/sched_average.c`, `compute_averages`, run every five seconds;
/// from the XNU source as remembered, flagged for verification against the open-source release). A read between two
/// recomputations returns the same figure, so asking more often costs a call and learns nothing.
pub const REFRESH_NS: u64 = 5_000_000_000;

/// Whether keeping a core takes none another thread wanted: some processor was free, as the kernel last averaged them.
/// `None` (the figure was refused, or the platform keeps no such count) keeps the core: there a harvest never sleeps,
/// or nothing tells the shard it would take a wanted core.
#[must_use]
pub fn core_free(free_processors: Option<u64>) -> bool {
  free_processors.is_none_or(|free| free > 0)
}

/// Whether an idle spin that began at `spin_began_ns` may still keep its core at `now_ns`: while a core is free
/// (`core_free`) and the spin is within what blocking costs, the shard's measured park and wake (`block_cost_ns`), the
/// competitive bound (see the module documentation).
#[must_use]
pub fn spin_keeps_core(
  core_free: bool,
  spin_began_ns: u64,
  now_ns: u64,
  block_cost_ns: u64,
) -> bool {
  core_free && now_ns.saturating_sub(spin_began_ns) < block_cost_ns
}

/// What a shard last learned of the kernel's free processors, and when.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeldCore {
  asked_ns: Option<u64>,
  free: bool,
}

impl HeldCore {
  /// Whether a core is free at `now_ns`: what the kernel last said, asked again through `ask` once [`REFRESH_NS`] has
  /// passed since the last asking (or never asked).
  pub fn core_free(&mut self, now_ns: u64, ask: impl FnOnce() -> Option<u64>) -> bool {
    let due = self
      .asked_ns
      .is_none_or(|asked| now_ns.saturating_sub(asked) >= REFRESH_NS);
    if due {
      self.free = core_free(ask());
      self.asked_ns = Some(now_ns);
    }
    self.free
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a kicked park's wake on Apple silicon (A-387's row), the bound a spin keeps its core within.
  const WAKE_NS: u64 = 5_000;

  /// 85a item 3. Do: ask with no processor free, one free, several free, and no figure. Expect: the core is kept with
  /// a free processor or no figure, and given up only when the kernel counted none free.
  #[test]
  fn a_core_is_kept_only_while_the_kernel_counts_one_free() {
    assert!(!core_free(Some(0)));
    assert!(core_free(Some(1)));
    assert!(core_free(Some(31)));
    assert!(core_free(None));
  }

  /// 85a item 3, the competitive bound. Do: ask a spin at its start, just inside the bound, at it, and past it, with a
  /// core free and with none. Expect: the core kept only inside the bound and only with a core free.
  #[test]
  fn a_spin_keeps_its_core_no_longer_than_blocking_costs_and_never_without_a_free_core() {
    let began = 1_000_000;
    assert!(spin_keeps_core(true, began, began, WAKE_NS));
    assert!(spin_keeps_core(true, began, began + WAKE_NS - 1, WAKE_NS));
    assert!(!spin_keeps_core(true, began, began + WAKE_NS, WAKE_NS));
    assert!(!spin_keeps_core(true, began, began + 10 * WAKE_NS, WAKE_NS));
    assert!(!spin_keeps_core(false, began, began, WAKE_NS));
    // A clock read before the spin's start (another shard's stamp) is no time spun.
    assert!(spin_keeps_core(true, began, began - 1, WAKE_NS));
    // No measured cost to block is no time to spin.
    assert!(!spin_keeps_core(true, began, began, 0));
  }

  /// 85a item 3, the cold path. Do: ask at once, again within the refresh interval with the kernel's answer changed,
  /// then once the interval has passed. Expect: the kernel asked the first time and once the interval passed, never
  /// between, and each answer followed.
  #[test]
  fn the_kernel_is_asked_at_most_once_a_refresh_interval() {
    let mut held = HeldCore::default();
    let asked = std::cell::Cell::new(0);
    let ask = |answer: Option<u64>| {
      asked.set(asked.get() + 1);
      answer
    };
    assert!(held.core_free(0, || ask(Some(2))));
    assert!(held.core_free(REFRESH_NS - 1, || ask(Some(0))));
    assert_eq!(asked.get(), 1);
    assert!(!held.core_free(REFRESH_NS, || ask(Some(0))));
    assert_eq!(asked.get(), 2);
    assert!(held.core_free(2 * REFRESH_NS, || ask(Some(4))));
    assert_eq!(asked.get(), 3);
  }
}
