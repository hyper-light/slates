//! Diagnostic only (g222, never main): the lane's own scheduling floor beside the bench (main's ask, 84a). Two threads
//! hand a token back and forth through `park`/`unpark` (a futex on Linux), each round trip timed on the monotonic
//! clock; under the same load the bench runs in, its percentiles are the floor any cross-thread wake meets there.
//! With `G222_FLOOR_TB` set (w-perf 84a, macOS), both threads take Mach's time-constraint policy from that window in ns,
//! as the daemon's shards and the client's reply wait do (A-831), so it is the control for the bench's own wakes.
#![allow(
  clippy::all,
  clippy::pedantic,
  clippy::restriction,
  missing_docs,
  clippy::disallowed_types,
  clippy::disallowed_methods
)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

fn main() {
  let rounds: usize = std::env::args()
    .nth(1)
    .and_then(|n| n.parse().ok())
    .unwrap_or(50_000);
  let spinners: usize = std::env::args()
    .nth(2)
    .and_then(|n| n.parse().ok())
    .unwrap_or(0);
  let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
  static TURN: AtomicU64 = AtomicU64::new(0);
  static STOP: AtomicBool = AtomicBool::new(false);
  let mut times = Vec::with_capacity(rounds);
  std::thread::scope(|scope| {
    for _ in 0..spinners * cores {
      scope.spawn(|| {
        while !STOP.load(Ordering::Relaxed) {
          std::hint::spin_loop();
        }
      });
    }
    let tb = std::env::var("G222_FLOOR_TB").ok().and_then(|v| v.parse::<u64>().ok());
    if let Some(window) = tb {
      let _ = slates_machine::time_constraint::set_current_from(window);
    }
    let main = std::thread::current();
    let echo = scope.spawn(move || {
      if let Some(window) = tb {
        let _ = slates_machine::time_constraint::set_current_from(window);
      }
      let mut seen = 0u64;
      loop {
        while TURN.load(Ordering::Acquire) == seen {
          if STOP.load(Ordering::Relaxed) {
            return;
          }
          std::thread::park();
        }
        seen = TURN.load(Ordering::Acquire) + 1;
        TURN.store(seen, Ordering::Release);
        main.unpark();
      }
    });
    for n in 0..rounds as u64 {
      let ping = 2 * n + 1;
      let started = Instant::now();
      TURN.store(ping, Ordering::Release);
      echo.thread().unpark();
      while TURN.load(Ordering::Acquire) == ping {
        std::thread::park();
      }
      times.push(started.elapsed().as_nanos() as u64);
    }
    STOP.store(true, Ordering::Relaxed);
    echo.thread().unpark();
  });
  times.sort_unstable();
  let pc = |p: f64| times[((times.len() as f64 * p) as usize).min(times.len() - 1)] as f64 / 1000.0;
  let band = times
    .iter()
    .filter(|t| (1_000_000..1_020_000).contains(*t))
    .count();
  println!(
    "floor: {} round trips, {} spinners/core: p50 {:.1} us, p99 {:.1} us, p999 {:.1} us, max {:.1} us, in [1.00, 1.02) ms {}",
    times.len(),
    spinners,
    pc(0.5),
    pc(0.99),
    pc(0.999),
    times.last().copied().unwrap_or(0) as f64 / 1000.0,
    band
  );
}
