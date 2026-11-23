//! The calling thread's own CPU time to the nanosecond on Windows (§4.3, the long-poll attribution, A-31), where the
//! times Windows keeps per thread (`GetThreadTimes`) are charged a scheduler tick at a time, about 15.6 ms, so a poll
//! past a quantum of microseconds reads no CPU at all. Until 2026-11-23 a Windows shard read no account, and every long
//! poll was unattributed (`NoClock`): 206 a quiet round on jupiter (A-871's diagnostic).
//!
//! - **The count:** `QueryThreadCycleTime` of the calling thread, the cycles it has run, which a blocked or preempted
//!   thread does not advance [B: Microsoft Learn, "QueryThreadCycleTime"]; `slates_rt`'s thread clock reads the same
//!   count of other threads for their progress.
//! - **The rate:** measured once a process ([`cycle_rate_hz`]): the cycles a spinning thread runs over a span of the
//!   host's interrupt time ([`crate::clock::monotonic_ns`]), the best of several spans, since a switch inside one only
//!   lowers it. Microsoft warns that the count's rate is the CPU's own and that some CPUs change it with their clock; a
//!   CPU whose counter keeps one rate whatever its clock (an invariant time-stamp counter, which x86_64 CPUs of the last
//!   decade have, from memory) reads true, and one that does not reads its CPU time at the measured rate, which only the
//!   attribution's split between a long run and a wait depends on.

use std::sync::OnceLock;

use windows_sys::Win32::System::Threading::GetCurrentThread;
use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

/// Format: nanoseconds per second.
const NS_PER_SECOND: u128 = 1_000_000_000;
/// Derived: a rate span is a thousand readings of the host clock's step ([`crate::clock::resolution_ns`], 100 ns on
/// Windows: a 100 µs span), so the clock's step is at most a thousandth of the span it measures.
const STEPS_A_SPAN: u64 = 1_000;
/// Shape: the spans the rate is the best of. A span reads low only when the thread is switched out inside it, so the
/// best of eight is low only on a host that runs the thread less than a span at a time for all eight in a row, about a
/// millisecond, where no CPU reading it takes means much.
const RATE_SPANS: usize = 8;

/// The calling thread's cycles so far; `None` when the call fails.
fn own_cycles() -> Option<u64> {
  let mut cycles = 0_u64;
  // SAFETY: the calling thread's pseudo-handle, which `QueryThreadCycleTime` accepts, and this frame's own count,
  // written by the call and read only when it answers success (nonzero).
  let answered = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) };
  (answered != 0).then_some(cycles)
}

/// The cycles a second the calling thread's count advances at while it runs, measured on the first call of the process
/// (see the module doc); `None` when the count cannot be read. A runtime asks it before its shards start, so no shard
/// spends the measurement's millisecond in a step.
pub fn cycle_rate_hz() -> Option<u64> {
  static RATE: OnceLock<Option<u64>> = OnceLock::new();
  *RATE.get_or_init(measure_rate)
}

/// The best rate over [`RATE_SPANS`] spans of [`STEPS_A_SPAN`] clock steps each.
fn measure_rate() -> Option<u64> {
  let span_ns = crate::clock::resolution_ns().saturating_mul(STEPS_A_SPAN);
  let mut best: Option<u128> = None;
  for _ in 0..RATE_SPANS {
    let began_cycles = own_cycles()?;
    let began_ns = crate::clock::monotonic_ns();
    let mut ended_ns = began_ns;
    while ended_ns.saturating_sub(began_ns) < span_ns {
      std::hint::spin_loop();
      ended_ns = crate::clock::monotonic_ns();
    }
    let ran = u128::from(own_cycles()?.saturating_sub(began_cycles));
    let rate = ran
      .saturating_mul(NS_PER_SECOND)
      .checked_div(u128::from(ended_ns.saturating_sub(began_ns)))?;
    best = best.max(Some(rate));
  }
  best
    .and_then(|rate| u64::try_from(rate).ok())
    .filter(|rate| *rate > 0)
}

/// The calling thread's CPU time so far, nanoseconds: its cycles at the measured rate. `None` when the count or the rate
/// cannot be read.
pub fn own_cpu_ns() -> Option<u64> {
  let rate = cycle_rate_hz()?;
  let cycles = own_cycles()?;
  u64::try_from(
    u128::from(cycles)
      .saturating_mul(NS_PER_SECOND)
      .checked_div(u128::from(rate))?,
  )
  .ok()
}

#[cfg(test)]
mod tests {
  use super::{cycle_rate_hz, own_cpu_ns};

  /// Shape: how long the test spins and sleeps, 20 ms: past a 15.6 ms scheduler tick, so neither fits between two.
  const SPAN: std::time::Duration = std::time::Duration::from_millis(20);

  /// A-31, by use: do read the thread's CPU, spin it for 20 ms of wall time, read again, sleep it 20 ms, read again.
  /// Expect a rate, the spin's CPU within its wall time and at least half of it (the runner may take the rest), and
  /// the sleep's CPU under a tenth of its wall time.
  #[test]
  #[allow(clippy::disallowed_methods)] // the test sleeps the thread on purpose: a block runs no cycles.
  fn a_spinning_thread_runs_its_wall_time_and_a_sleeping_one_none() {
    assert!(cycle_rate_hz().is_some(), "no cycle rate measured");
    let before = own_cpu_ns().unwrap();
    let began = std::time::Instant::now();
    while began.elapsed() < SPAN {
      std::hint::spin_loop();
    }
    let spun_wall = u64::try_from(began.elapsed().as_nanos()).unwrap();
    let spun = own_cpu_ns().unwrap() - before;
    std::thread::sleep(SPAN);
    let slept = own_cpu_ns().unwrap() - before - spun;
    println!(
      "own_cpu: rate {:?} Hz; spun {spun} ns of {spun_wall} ns wall; slept {slept} ns",
      cycle_rate_hz()
    );
    assert!(
      spun <= spun_wall + spun_wall / 20 && spun >= spun_wall / 2,
      "spun {spun} ns of CPU in {spun_wall} ns"
    );
    assert!(
      slept < u64::try_from(SPAN.as_nanos()).unwrap() / 10,
      "slept {slept} ns of CPU"
    );
  }
}
