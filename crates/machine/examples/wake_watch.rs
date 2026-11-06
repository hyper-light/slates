//! Diagnostics only, never on main (A-223): what `QueryThreadCycleTime` shows of another thread, and the wake probe
//! this tree builds. (1) A spinning thread read twice back to back: how often its count stood still (a confirmation
//! the probe would wrongly take). (2) A parked thread read twice across a span: how often it moved. (3) The remote read
//! against the thread's own: whether a running thread's remote count lags its own. (4) Five wake probes at the
//! profile's budget. (5) The waiter's own context-switch count (`ReadThreadProfilingData`) across a wait on an I/O
//! completion port, as the shard waits: posted right after the waiter's announcement (A), and only once its cycle
//! count stood still across two reads (B); each wake split by whether the waiter switched out during the wait.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::disallowed_methods,
  clippy::undocumented_unsafe_blocks,
  clippy::field_reassign_with_default,
  clippy::cognitive_complexity,
  unreachable_pub,
  unused_imports,
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
mod iocp {
  use std::sync::atomic::{AtomicU64, Ordering};
  use std::time::Instant;
  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
  use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED, PostQueuedCompletionStatus,
  };
  use windows_sys::Win32::System::Performance::HardwareCounterProfiling::{
    DisableThreadProfiling, EnableThreadProfiling, PERFORMANCE_DATA, ReadThreadProfilingData,
  };
  use windows_sys::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, INFINITE, OpenThread, SetThreadAffinityMask,
    THREAD_QUERY_LIMITED_INFORMATION,
  };
  use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

  const THREAD_PROFILING_FLAG_DISPATCH: u32 = 1;
  const READ_THREAD_PROFILING_FLAG_DISPATCHING: u32 = 1;
  const PERFORMANCE_DATA_VERSION: u8 = 1;
  const SAMPLES: usize = 2_000;

  static ANNOUNCED: AtomicU64 = AtomicU64::new(0);
  static WOKE_NS: AtomicU64 = AtomicU64::new(0);
  static SWITCHED: AtomicU64 = AtomicU64::new(0);
  static WAIT_BITS: AtomicU64 = AtomicU64::new(0);
  static TID: AtomicU64 = AtomicU64::new(0);
  static PORT: AtomicU64 = AtomicU64::new(0);
  static READ_NS: AtomicU64 = AtomicU64::new(0);

  fn ns(base: Instant) -> u64 {
    u64::try_from(base.elapsed().as_nanos()).unwrap()
  }

  fn read(h: HANDLE) -> PERFORMANCE_DATA {
    let mut pd = PERFORMANCE_DATA::default();
    pd.Size = u16::try_from(std::mem::size_of::<PERFORMANCE_DATA>()).unwrap();
    pd.Version = PERFORMANCE_DATA_VERSION;
    let rc = unsafe { ReadThreadProfilingData(h, READ_THREAD_PROFILING_FLAG_DISPATCHING, &mut pd) };
    assert_eq!(rc, 0, "ReadThreadProfilingData answered {rc}");
    pd
  }

  fn waiter(base: Instant, cpu: Option<u32>) {
    unsafe {
      if let Some(c) = cpu {
        SetThreadAffinityMask(GetCurrentThread(), 1usize << c);
      }
      let mut prof: HANDLE = std::ptr::null_mut();
      let rc = EnableThreadProfiling(GetCurrentThread(), THREAD_PROFILING_FLAG_DISPATCH, 0, &mut prof);
      assert_eq!(rc, 0, "EnableThreadProfiling answered {rc}");
      let t = Instant::now();
      for _ in 0..1000 {
        std::hint::black_box(read(prof));
      }
      READ_NS.store(u64::try_from(t.elapsed().as_nanos()).unwrap() / 1000, Ordering::Release);
      TID.store(u64::from(GetCurrentThreadId()), Ordering::Release);
      let port = PORT.load(Ordering::Acquire) as HANDLE;
      loop {
        let before = read(prof);
        ANNOUNCED.store(1, Ordering::Release);
        let (mut bytes, mut key) = (0u32, 0usize);
        let mut ov: *mut OVERLAPPED = std::ptr::null_mut();
        GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut ov, INFINITE);
        let now = ns(base);
        let after = read(prof);
        if key == 2 {
          DisableThreadProfiling(prof);
          return;
        }
        SWITCHED.store(u64::from(after.ContextSwitchCount - before.ContextSwitchCount), Ordering::Release);
        WAIT_BITS.store(after.WaitReasonBitMap, Ordering::Release);
        WOKE_NS.store(now, Ordering::Release);
      }
    }
  }

  fn cycles(thread: HANDLE) -> u64 {
    let mut c = 0u64;
    unsafe { QueryThreadCycleTime(thread, &mut c) };
    c
  }

  fn pct(v: &mut [u64], p: usize) -> u64 {
    if v.is_empty() {
      return 0;
    }
    v.sort_unstable();
    v[(v.len() - 1) * p / 100]
  }

  fn mean(v: &[u64]) -> u64 {
    if v.is_empty() { 0 } else { v.iter().sum::<u64>() / u64::try_from(v.len()).unwrap() }
  }

  pub fn run(pin: Option<(u32, u32)>) {
    let base = Instant::now();
    unsafe {
      let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1);
      PORT.store(port as u64, Ordering::Release);
      if let Some((waker, _)) = pin {
        SetThreadAffinityMask(GetCurrentThread(), 1usize << waker);
      }
      TID.store(0, Ordering::Release);
      let w = std::thread::spawn(move || waiter(base, pin.map(|(_, waiter)| waiter)));
      while TID.load(Ordering::Acquire) == 0 {
        std::hint::spin_loop();
      }
      let thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, u32::try_from(TID.load(Ordering::Acquire)).unwrap());
      // [mode][switched] latencies; mode 0 = A (announced), 1 = B (cycles still).
      let mut lat: [[Vec<u64>; 2]; 2] = Default::default();
      let mut moved_at_post = [0usize; 2];
      let mut gave_up = 0usize;
      let mut bits = 0u64;
      for i in 0..SAMPLES * 2 {
        let t = Instant::now();
        while t.elapsed() < std::time::Duration::from_micros(200) {
          std::thread::yield_now();
        }
        while ANNOUNCED.load(Ordering::Acquire) == 0 {
          std::hint::spin_loop();
        }
        let mode = i % 2;
        let mut still = cycles(thread);
        if mode == 1 {
          let mut ok = false;
          for _ in 0..100_000 {
            let first = cycles(thread);
            let second = cycles(thread);
            if first == second {
              still = second;
              ok = true;
              break;
            }
          }
          if !ok {
            gave_up += 1;
          }
        }
        ANNOUNCED.store(0, Ordering::Release);
        WOKE_NS.store(0, Ordering::Release);
        if cycles(thread) != still {
          moved_at_post[mode] += 1;
        }
        let t0 = ns(base);
        PostQueuedCompletionStatus(port, 0, 1, std::ptr::null_mut());
        let woke = loop {
          let w = WOKE_NS.load(Ordering::Acquire);
          if w != 0 {
            break w;
          }
          std::hint::spin_loop();
        };
        let switched = SWITCHED.load(Ordering::Acquire) > 0;
        bits |= WAIT_BITS.load(Ordering::Acquire);
        lat[mode][usize::from(switched)].push(woke.saturating_sub(t0));
      }
      PostQueuedCompletionStatus(port, 0, 2, std::ptr::null_mut());
      w.join().unwrap();
      CloseHandle(thread);
      CloseHandle(port);
      let label = pin.map_or("unpinned".to_owned(), |(a, b)| format!("pinned waker {a} waiter {b}"));
      for (mode, name) in [(0usize, "A announced"), (1, "B cycles-still")] {
        let [mut unswitched, mut switched] = std::mem::take(&mut lat[mode]);
        println!(
          "iocp {label} {name}: switched {} (p50 {} p99 {} mean {} ns), not switched {} (p50 {} p99 {} mean {} ns), cycles moved at post {}{}",
          switched.len(), pct(&mut switched, 50), pct(&mut switched, 99), mean(&switched),
          unswitched.len(), pct(&mut unswitched, 50), pct(&mut unswitched, 99), mean(&unswitched),
          moved_at_post[mode],
          if mode == 1 { format!(", gave up {gave_up}") } else { String::new() }
        );
      }
      println!("iocp {label}: ReadThreadProfilingData {} ns a read; wait-reason bits seen {bits:#x}", READ_NS.load(Ordering::Acquire));
    }
  }
}

#[cfg(windows)]
fn cycle_semantics() {
  iocp::run(None);
  iocp::run(Some((1, 2)));
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
