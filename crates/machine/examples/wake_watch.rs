//! Diagnostics only, never on main (A-223): what `QueryThreadCycleTime` shows of another thread, and the wake probe
//! this tree builds. (1) A spinning thread read twice back to back: how often its count stood still (a confirmation
//! the probe would wrongly take). (2) A parked thread read twice across a span: how often it moved. (3) The remote read
//! against the thread's own: whether a running thread's remote count lags its own. (4) Five wake probes at the
//! profile's budget.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::disallowed_methods,
  clippy::undocumented_unsafe_blocks,
  unsafe_code
)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PAIRS: usize = 100_000;
const PARKED_PAIRS: usize = 2_000;
const PROBES: usize = 5;

#[cfg(windows)]
mod watch {
  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
  use windows_sys::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
  };
  use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

  pub struct Watch(HANDLE);
  pub fn id() -> u32 {
    unsafe { GetCurrentThreadId() }
  }
  pub fn own() -> u64 {
    let mut c = 0u64;
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
    c
  }
  impl Watch {
    pub fn open(id: u32) -> Watch {
      Watch(unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, id) })
    }
    pub fn cycles(&self) -> u64 {
      let mut c = 0u64;
      unsafe { QueryThreadCycleTime(self.0, &mut c) };
      c
    }
  }
  impl Drop for Watch {
    fn drop(&mut self) {
      unsafe { CloseHandle(self.0) };
    }
  }
}

#[cfg(windows)]
fn cycle_semantics() {
  // (1) and (3): a spinner, its own count published as it spins.
  let stop = AtomicBool::new(false);
  let tid = AtomicU64::new(0);
  let own = AtomicU64::new(0);
  std::thread::scope(|scope| {
    scope.spawn(|| {
      tid.store(u64::from(watch::id()), Ordering::Release);
      while !stop.load(Ordering::Acquire) {
        own.store(watch::own(), Ordering::Release);
      }
    });
    while tid.load(Ordering::Acquire) == 0 {
      std::hint::spin_loop();
    }
    let w = watch::Watch::open(u32::try_from(tid.load(Ordering::Acquire)).unwrap());
    let mut still = 0usize;
    let mut lagged = 0usize;
    let mut read_ns = Vec::with_capacity(PAIRS);
    for _ in 0..PAIRS {
      let own_before = own.load(Ordering::Acquire);
      let t = Instant::now();
      let first = w.cycles();
      read_ns.push(u64::try_from(t.elapsed().as_nanos()).unwrap());
      let second = w.cycles();
      if first == second {
        still += 1;
      }
      if first < own_before {
        lagged += 1;
      }
    }
    stop.store(true, Ordering::Release);
    read_ns.sort_unstable();
    println!(
      "watch.spinner: {still} of {PAIRS} back-to-back read pairs stood still; {lagged} remote reads behind the thread's own; one read p50 {} ns p99 {} ns",
      read_ns[PAIRS / 2],
      read_ns[PAIRS * 99 / 100]
    );
  });
  // (2) a parked thread, read across 100 µs spans.
  let stop = AtomicBool::new(false);
  let tid = AtomicU64::new(0);
  std::thread::scope(|scope| {
    let sleeper = scope.spawn(|| {
      tid.store(u64::from(watch::id()), Ordering::Release);
      while !stop.load(Ordering::Acquire) {
        std::thread::park();
      }
    });
    while tid.load(Ordering::Acquire) == 0 {
      std::hint::spin_loop();
    }
    std::thread::sleep(Duration::from_millis(20));
    let w = watch::Watch::open(u32::try_from(tid.load(Ordering::Acquire)).unwrap());
    let mut moved = 0usize;
    for _ in 0..PARKED_PAIRS {
      let first = w.cycles();
      let t = Instant::now();
      while t.elapsed() < Duration::from_micros(100) {
        std::hint::spin_loop();
      }
      if w.cycles() != first {
        moved += 1;
      }
    }
    stop.store(true, Ordering::Release);
    sleeper.thread().unpark();
    println!("watch.parked: {moved} of {PARKED_PAIRS} read pairs 100 us apart moved");
  });
}

#[cfg(not(windows))]
fn cycle_semantics() {
  println!("watch: no cycle-count watch on this platform");
}

fn main() {
  cycle_semantics();
  let facts = slates_machine::facts::Facts::query();
  let placement = slates_machine::placement::Placement::of(&facts.cores, facts.cpu_budget);
  for probe in 0..PROBES {
    match slates_machine::wake::wake(slates_machine::bench::PROBE_WALL_BUDGET, &placement) {
      Ok(w) => println!("wake.probe {probe}: {w:?}"),
      Err(e) => println!("wake.probe {probe}: refused {e:?}"),
    }
  }
}
