//! What this build costs against the machine (§4.14, A-318): the same work timed as this build compiled it and as
//! machine code no build changes, so a judgment made in CPU time is held to the build it runs in.
//!
//! The liveness watchdog calls a shard spinning once it has run a window of CPU without completing a step
//! (`slates_server::watchdog`, A-228): a cooperative task yields within the shard's step budget, microseconds, so a
//! window of CPU is no poll's. That holds for the build the window was set for. A build that runs the same code slower
//! stretches every poll by its slowdown: ThreadSanitizer calls into its runtime at every memory access and every
//! function's entry and exit, and an unoptimized build keeps every helper a call. On CI's ThreadSanitizer lane a shard
//! ran 1,054 ms of CPU in one poll (run 44238377555) and another 1,060 ms (diag-b13-2, 2026-11-18), each over the
//! one-second window, and the watchdog of an in-process daemon, which no anchor reads, ended the whole test binary.
//!
//! The measure is one piece of work written twice: a dependent chain over one page of words, `acc = acc × K + word`,
//! once in Rust, compiled by this build, and once in the machine's own instructions (`asm!`), which no compiler flag
//! instruments or deoptimizes, and which costs what the shipped build's chain costs (both are a load, a multiply and an
//! add a word, the multiply's latency bounding each). Each is timed in batches long enough for the clock
//! ([`crate::bench::TIMER_OVERHEAD_FACTOR`] times its measured overhead), the two alternately, [`crate::bench::MIN_SAMPLES`]
//! rounds, and the least batch of each is kept, so a preemption or a move between cores taints neither. The two must
//! compute the same value, or the measure is refused: a machine-code chain that disagrees is wrong, and its time says
//! nothing. The build's slowdown is compiled ÷ machine code, never below one ([`BuildCost::scale`]).
//!
//! Miri runs no machine code ([`BuildCostRefused::NoMachineCode`]); it has no thread clock either, and the watchdog
//! judges there by the wall alone.

use std::hint::black_box;
use std::time::Instant;

use crate::bench::{MIN_SAMPLES, TIMER_OVERHEAD_FACTOR, nanos, timer_overhead_ns};

/// Format: the chain's multiplier, odd so the chain never collapses to zero: the 64-bit golden-ratio constant of
/// Knuth's multiplicative hashing (TAOCP vol. 3, §6.4).
const MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;

/// Shape: the chain's length in words, one page of the smallest page any target has (4 KiB), so the chain's loads stay
/// in the first-level cache and neither version waits on memory.
const CHAIN_WORDS: usize = 512;

/// Format: the chain's starting value, any odd word.
const SEED: u64 = 1;

/// This build's cost against the machine's for the same work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildCost {
  /// The least time of a batch of chains as this build compiled them, nanoseconds.
  pub compiled_ns: u64,
  /// The least time of the same batch in machine code, nanoseconds.
  pub machine_ns: u64,
  /// The chains a batch ran.
  pub batch: u32,
}

impl BuildCost {
  /// `ns` held to this build: scaled by compiled ÷ machine code, never below `ns` (a build that runs the chain as fast as
  /// machine code, or faster, keeps the time as it is).
  pub fn scale(&self, ns: u64) -> u64 {
    let scaled = u128::from(ns)
      .saturating_mul(u128::from(self.compiled_ns))
      .checked_div(u128::from(self.machine_ns))
      .unwrap_or(u128::from(ns));
    u64::try_from(scaled).unwrap_or(u64::MAX).max(ns)
  }

  /// The slowdown in tenths (compiled ÷ machine code × 10, at least 10), for a log line.
  pub fn tenths(&self) -> u64 {
    /// Format: tenths in one.
    const TENTHS: u64 = 10;
    self.scale(TENTHS)
  }
}

/// Why the build's cost was not measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildCostRefused {
  /// No machine-code chain runs here (Miri interprets the program and runs no machine code).
  NoMachineCode,
  /// The two chains computed different values: the machine-code chain is wrong, so its time says nothing.
  Disagreed {
    /// The compiled chain's value.
    compiled: u64,
    /// The machine-code chain's value.
    machine: u64,
  },
  /// No batch of machine-code chains reached the clock's resolution, however many chains it ran.
  Unmeasurable,
}

/// The chain as this build compiles it.
#[inline(never)]
fn compiled_chain(words: &[u64], seed: u64) -> u64 {
  let mut acc = seed;
  for word in words {
    acc = acc.wrapping_mul(MULTIPLIER).wrapping_add(*word);
  }
  acc
}

/// The chain in x86-64 instructions: a multiply, an add from memory, a step a word.
#[cfg(all(target_arch = "x86_64", not(miri)))]
#[inline(never)]
fn machine_chain(words: &[u64], seed: u64) -> Result<u64, BuildCostRefused> {
  let mut acc = seed;
  if words.is_empty() {
    return Ok(acc);
  }
  // SAFETY: the loop reads `words.len()` words from `words.as_ptr()` onward, all inside the slice, which the caller holds
  // for the call; the count is at least one, so the loop ends after exactly that many words; the block writes only the
  // registers it names and the flags, touches no stack, writes no memory and calls nothing.
  unsafe {
    core::arch::asm!(
      "2:",
      "imul {acc}, {multiplier}",
      "add {acc}, qword ptr [{at}]",
      "add {at}, 8",
      "dec {left}",
      "jnz 2b",
      acc = inout(reg) acc,
      multiplier = in(reg) MULTIPLIER,
      at = inout(reg) words.as_ptr() => _,
      left = inout(reg) words.len() => _,
      options(nostack, readonly),
    );
  }
  Ok(acc)
}

/// The chain in AArch64 instructions: a load with post-increment, a multiply-add, a step a word.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline(never)]
fn machine_chain(words: &[u64], seed: u64) -> Result<u64, BuildCostRefused> {
  let mut acc = seed;
  if words.is_empty() {
    return Ok(acc);
  }
  // SAFETY: the loop reads `words.len()` words from `words.as_ptr()` onward, all inside the slice, which the caller holds
  // for the call; the count is at least one, so the loop ends after exactly that many words; the block writes only the
  // registers it names and the flags, touches no stack, writes no memory and calls nothing.
  unsafe {
    core::arch::asm!(
      "2:",
      "ldr {word}, [{at}], #8",
      "madd {acc}, {acc}, {multiplier}, {word}",
      "subs {left}, {left}, #1",
      "b.ne 2b",
      acc = inout(reg) acc,
      multiplier = in(reg) MULTIPLIER,
      word = out(reg) _,
      at = inout(reg) words.as_ptr() => _,
      left = inout(reg) words.len() => _,
      options(nostack, readonly),
    );
  }
  Ok(acc)
}

/// No machine-code chain: Miri runs none, and slates targets x86-64 and AArch64 alone.
#[cfg(any(miri, not(any(target_arch = "x86_64", target_arch = "aarch64"))))]
fn machine_chain(_words: &[u64], _seed: u64) -> Result<u64, BuildCostRefused> {
  Err(BuildCostRefused::NoMachineCode)
}

/// Runs `chain` over `words` `batch` times, each from the last one's value; the time it took and the value.
fn timed_batch(
  chain: fn(&[u64], u64) -> Result<u64, BuildCostRefused>,
  words: &[u64],
  batch: u32,
) -> Result<(u64, u64), BuildCostRefused> {
  let began = Instant::now();
  let mut acc = SEED;
  for _ in 0..batch {
    acc = chain(black_box(words), black_box(acc))?;
  }
  Ok((nanos(began.elapsed()), black_box(acc)))
}

/// The compiled chain in the shape [`timed_batch`] takes.
fn compiled(words: &[u64], seed: u64) -> Result<u64, BuildCostRefused> {
  Ok(compiled_chain(words, seed))
}

/// Measures this build's cost against the machine's: the least batch of the compiled chain and of the machine-code
/// chain over [`MIN_SAMPLES`] alternate rounds, a batch long enough that the machine-code one lasts
/// [`TIMER_OVERHEAD_FACTOR`] times the clock's overhead. A few milliseconds in an unoptimized build, tens under
/// ThreadSanitizer.
pub fn measure_build_cost() -> Result<BuildCost, BuildCostRefused> {
  let words: Vec<u64> = (0..CHAIN_WORDS)
    .map(|index| {
      u64::try_from(index)
        .unwrap_or(u64::MAX)
        .wrapping_mul(MULTIPLIER)
    })
    .collect();
  let floor = timer_overhead_ns()
    .saturating_mul(TIMER_OVERHEAD_FACTOR)
    .max(1);
  let mut batch: u32 = 1;
  loop {
    let (elapsed, _) = timed_batch(machine_chain, &words, batch)?;
    if elapsed >= floor {
      break;
    }
    batch = batch.checked_mul(2).ok_or(BuildCostRefused::Unmeasurable)?;
  }
  let mut compiled_ns = u64::MAX;
  let mut machine_ns = u64::MAX;
  for _ in 0..MIN_SAMPLES {
    let (machine_took, machine_value) = timed_batch(machine_chain, &words, batch)?;
    let (compiled_took, compiled_value) = timed_batch(compiled, &words, batch)?;
    if machine_value != compiled_value {
      return Err(BuildCostRefused::Disagreed {
        compiled: compiled_value,
        machine: machine_value,
      });
    }
    machine_ns = machine_ns.min(machine_took);
    compiled_ns = compiled_ns.min(compiled_took);
  }
  Ok(BuildCost {
    compiled_ns,
    machine_ns: machine_ns.max(1),
    batch,
  })
}

/// This build's cost against the machine's, measured once a process ([`measure_build_cost`]): the binary does not change
/// while it runs, so every daemon of a process holds its judgments to the one measure.
pub fn build_cost() -> Result<BuildCost, BuildCostRefused> {
  static MEASURED: std::sync::OnceLock<Result<BuildCost, BuildCostRefused>> =
    std::sync::OnceLock::new();
  *MEASURED.get_or_init(measure_build_cost)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-318. Do: run both chains over the same words from the same seed, one word, a page, and none. Expect: the same
  /// value each time, and the page's value not the seed's, so the machine-code chain did the compiled one's work.
  #[cfg(not(miri))]
  #[test]
  fn the_machine_code_chain_computes_what_the_compiled_one_does() {
    let words: Vec<u64> = (0..CHAIN_WORDS)
      .map(|index| u64::try_from(index).unwrap().wrapping_mul(MULTIPLIER))
      .collect();
    for length in [0, 1, CHAIN_WORDS] {
      let slice = &words[..length];
      assert_eq!(
        machine_chain(slice, SEED),
        Ok(compiled_chain(slice, SEED)),
        "{length} words"
      );
    }
    assert_ne!(compiled_chain(&words, SEED), SEED, "the page's chain moved");
  }

  /// A-318. Do: measure the build's cost. Expect: both chains timed, each batch at least the clock's floor; an
  /// unoptimized build (this test's, unless run with `--release`) measured at least twice as slow as machine code,
  /// since it keeps the iterator, the multiply and the add as calls; and `scale` never shrinks a time.
  #[cfg(not(miri))]
  #[test]
  fn an_unoptimized_build_measures_slower_than_machine_code() {
    let cost = measure_build_cost().unwrap();
    assert!(cost.machine_ns > 0 && cost.compiled_ns > 0, "{cost:?}");
    if cfg!(debug_assertions) {
      assert!(
        cost.scale(1_000) >= 2_000,
        "an unoptimized build runs the chain at least twice as slow as machine code: {cost:?}"
      );
    }
    assert!(cost.scale(1_000) >= 1_000, "{cost:?}");
    assert!(cost.tenths() >= 10, "{cost:?}");
  }

  /// A-318. Do: scale times by costs of equal, double and lesser compiled time, and by a cost whose product overflows.
  /// Expect: the time kept, doubled, kept (never shrunk), and saturated.
  #[test]
  fn scaling_holds_a_time_to_the_build_never_below_it() {
    let cost = |compiled_ns, machine_ns| BuildCost {
      compiled_ns,
      machine_ns,
      batch: 1,
    };
    assert_eq!(cost(100, 100).scale(1_000), 1_000);
    assert_eq!(cost(200, 100).scale(1_000), 2_000);
    assert_eq!(cost(50, 100).scale(1_000), 1_000);
    assert_eq!(cost(u64::MAX, 1).scale(u64::MAX), u64::MAX);
    assert_eq!(cost(250, 100).tenths(), 25);
  }

  /// A-318, Miri. Do: measure under Miri. Expect: refused for want of machine code, never a made-up cost.
  #[cfg(miri)]
  #[test]
  fn miri_runs_no_machine_code() {
    assert_eq!(measure_build_cost(), Err(BuildCostRefused::NoMachineCode));
  }
}
