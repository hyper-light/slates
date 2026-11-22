//! w-perf 84a (diagnostic, never main): what a macOS address wake costs its waker under load. Each waker wakes its
//! waiters in turn: it bumps a waiter's word, wakes it, times the wake call's wall and thread-CPU time, and waits for
//! the waiter's acknowledgement before the next. Variants (environment): `PROBE_SHARED=1` the shared-address flags (the
//! daemon's reply wake) else the private ones; `PROBE_WAITER_TB=1` and `PROBE_WAKER_TB=1` the time-constraint policy
//! (A-831's terms, 50 µs) on that side; `PROBE_WAKERS` (default 2) and `PROBE_WAITERS` per waker (default 4); the first
//! argument the wakes per waker, the second the spinners per core. `PROBE_MECH` is the wake: `ulock` (default, the
//! address wake), `sem` (a Mach semaphore, what a cross-process semaphore_signal does) or `pipe` (a byte written to the
//! waiter's pipe, its read blocking: the kevent-on-a-descriptor path). `PROBE_CHURN=1` adds a thread mapping, touching
//! and unmapping 1 MiB in a loop, as the daemon's arenas do.
#![allow(
  clippy::all,
  clippy::pedantic,
  clippy::restriction,
  missing_docs,
  clippy::disallowed_types,
  clippy::disallowed_methods
)]

#[cfg(not(target_os = "macos"))]
fn main() {
  println!("probe: macOS only");
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
  static mach_task_self_: u32;
  fn semaphore_create(task: u32, semaphore: *mut u32, policy: i32, value: i32) -> i32;
  fn semaphore_signal(semaphore: u32) -> i32;
  fn semaphore_timedwait(semaphore: u32, wait_time: MachTimespec) -> i32;
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct MachTimespec {
  tv_sec: u32,
  tv_nsec: i32,
}

#[cfg(target_os = "macos")]
fn main() {
  use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
  let wakes: usize = std::env::args()
    .nth(1)
    .and_then(|n| n.parse().ok())
    .unwrap_or(20_000);
  let spinners: usize = std::env::args()
    .nth(2)
    .and_then(|n| n.parse().ok())
    .unwrap_or(2);
  let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1");
  let shared = flag("PROBE_SHARED");
  let waiter_tb = flag("PROBE_WAITER_TB");
  let waker_tb = flag("PROBE_WAKER_TB");
  let wakers: usize = std::env::var("PROBE_WAKERS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(2);
  let per: usize = std::env::var("PROBE_WAITERS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(4);
  let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
  let waiters = wakers * per;
  let mech = std::env::var("PROBE_MECH").unwrap_or_else(|_| "ulock".to_owned());
  let churn = flag("PROBE_CHURN");
  let sems: Vec<u32> = (0..waiters)
    .map(|_| {
      let mut sem = 0u32;
      // SAFETY: this task's own port and an out-parameter on this frame.
      let rc = unsafe { semaphore_create(mach_task_self_, &mut sem, 0, 0) };
      assert_eq!(rc, 0, "semaphore_create");
      sem
    })
    .collect();
  let pipes: Vec<(i32, i32)> = (0..waiters)
    .map(|_| {
      let mut fds = [0i32; 2];
      // SAFETY: a two-int array on this frame.
      assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
      (fds[0], fds[1])
    })
    .collect();
  let sems = &sems;
  let pipes = &pipes;
  let mech = mech.as_str();
  // Words and acknowledgements in one shared anonymous mapping, a page apart as the daemon's client regions are.
  let page = 16_384usize;
  // SAFETY: an anonymous shared mapping of the stated length; checked below.
  let base = unsafe {
    libc::mmap(
      std::ptr::null_mut(),
      page * waiters * 2,
      libc::PROT_READ | libc::PROT_WRITE,
      libc::MAP_SHARED | libc::MAP_ANON,
      -1,
      0,
    )
  };
  assert!(base != libc::MAP_FAILED, "mmap");
  let base = base as usize;
  let word = |i: usize| -> &'static AtomicU32 {
    // SAFETY: inside the mapping, aligned, zeroed, live for the process.
    unsafe { &*((base as *mut u8).add(i * 2 * page).cast::<AtomicU32>()) }
  };
  let ack = |i: usize| -> &'static AtomicU32 {
    // SAFETY: as above, the second page of the pair.
    unsafe {
      &*((base as *mut u8)
        .add((i * 2 + 1) * page)
        .cast::<AtomicU32>())
    }
  };
  let (wait_flags, wake_flags) = if shared {
    (
      libc::OS_SYNC_WAIT_ON_ADDRESS_SHARED,
      libc::OS_SYNC_WAKE_BY_ADDRESS_SHARED,
    )
  } else {
    (
      libc::OS_SYNC_WAIT_ON_ADDRESS_NONE,
      libc::OS_SYNC_WAKE_BY_ADDRESS_NONE,
    )
  };
  static STOP: AtomicBool = AtomicBool::new(false);
  let cpu = || {
    let reading = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    reading.tv_sec as u64 * 1_000_000_000 + reading.tv_nsec as u64
  };
  let mono = || {
    let reading = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    reading.tv_sec as u64 * 1_000_000_000 + reading.tv_nsec as u64
  };
  let rows: Vec<Vec<(u64, u64)>> = std::thread::scope(|scope| {
    for _ in 0..spinners * cores {
      scope.spawn(|| {
        while !STOP.load(Ordering::Relaxed) {
          std::hint::spin_loop();
        }
      });
    }
    if churn {
      scope.spawn(|| {
        while !STOP.load(Ordering::Relaxed) {
          // SAFETY: an anonymous private mapping, touched page by page, then unmapped.
          unsafe {
            let len = 1 << 20;
            let at = libc::mmap(
              std::ptr::null_mut(),
              len,
              libc::PROT_READ | libc::PROT_WRITE,
              libc::MAP_PRIVATE | libc::MAP_ANON,
              -1,
              0,
            );
            if at != libc::MAP_FAILED {
              for off in (0..len).step_by(16_384) {
                at.cast::<u8>().add(off).write_volatile(1);
              }
              libc::munmap(at, len);
            }
          }
        }
      });
    }
    for i in 0..waiters {
      scope.spawn(move || {
        if waiter_tb {
          let _ = slates_machine::time_constraint::set_current_from(50_000);
        }
        let w = word(i);
        let mut seen = 0u32;
        while !STOP.load(Ordering::Relaxed) {
          if mech == "sem" {
            // SAFETY: this waiter's own semaphore; a bounded wait so a stop is seen.
            unsafe {
              semaphore_timedwait(
                sems[i],
                MachTimespec {
                  tv_sec: 0,
                  tv_nsec: 100_000_000,
                },
              )
            };
          } else if mech == "pipe" {
            let mut byte = [0u8; 1];
            // SAFETY: this waiter's own pipe and a one-byte buffer on this frame.
            unsafe { libc::read(pipes[i].0, byte.as_mut_ptr().cast(), 1) };
          }
          while mech == "ulock"
            && w.load(Ordering::Acquire) == seen
            && !STOP.load(Ordering::Relaxed)
          {
            // SAFETY: a live word of the mapping; bounded wait so a stop is seen.
            unsafe {
              libc::os_sync_wait_on_address_with_timeout(
                w.as_ptr().cast(),
                u64::from(seen),
                4,
                wait_flags,
                libc::OS_CLOCK_MACH_ABSOLUTE_TIME,
                100_000_000,
              );
            }
          }
          seen = w.load(Ordering::Acquire);
          ack(i).store(seen, Ordering::Release);
        }
      });
    }
    let handles: Vec<_> = (0..wakers)
      .map(|k| {
        scope.spawn(move || {
          if waker_tb {
            let _ = slates_machine::time_constraint::set_current_from(50_000);
          }
          let mut out = Vec::with_capacity(wakes);
          for n in 0..wakes {
            let i = k * per + n % per;
            let next = word(i).load(Ordering::Relaxed) + 1;
            // Give the waiter time to sleep again, as a client parks between requests.
            let settle = mono();
            while mono() - settle < 50_000 {
              std::hint::spin_loop();
            }
            word(i).store(next, Ordering::Release);
            let (w0, c0) = (mono(), cpu());
            // SAFETY: a live word of the mapping.
            unsafe {
              match mech {
                "sem" => {
                  semaphore_signal(sems[i]);
                }
                "pipe" => {
                  libc::write(pipes[i].1, [1u8].as_ptr().cast(), 1);
                }
                _ => {
                  libc::os_sync_wake_by_address_any(word(i).as_ptr().cast(), 4, wake_flags);
                }
              }
            }
            out.push((mono() - w0, cpu() - c0));
            while ack(i).load(Ordering::Acquire) != next {
              std::thread::yield_now();
            }
          }
          out
        })
      })
      .collect();
    let rows = handles.into_iter().map(|h| h.join().unwrap()).collect();
    STOP.store(true, Ordering::Relaxed);
    for i in 0..waiters {
      word(i).fetch_add(1, Ordering::Release);
      // SAFETY: as above.
      unsafe {
        libc::os_sync_wake_by_address_all(word(i).as_ptr().cast(), 4, wake_flags);
        semaphore_signal(sems[i]);
        libc::write(pipes[i].1, [1u8].as_ptr().cast(), 1);
      }
    }
    rows
  });
  let mut walls: Vec<u64> = rows.iter().flatten().map(|r| r.0).collect();
  walls.sort_unstable();
  let n = walls.len();
  let q = |p: f64| walls[((n as f64 * p) as usize).min(n - 1)] as f64 / 1e3;
  let long: Vec<&(u64, u64)> = rows.iter().flatten().filter(|r| r.0 >= 1_000_000).collect();
  let on_cpu = long.iter().filter(|r| r.1 * 10 >= r.0 * 9).count();
  println!(
    "probe mech={} churn={} shared={} waker_tb={} waiter_tb={} wakers={} waiters={} spinners/core={}: wake wall p50 {:.1} p99 {:.1} p999 {:.1} max {:.1} us; >=1 ms {} ({} on CPU >=90%)",
    mech,
    u8::from(churn),
    u8::from(shared),
    u8::from(waker_tb),
    u8::from(waiter_tb),
    wakers,
    waiters,
    spinners,
    q(0.5),
    q(0.99),
    q(0.999),
    walls[n - 1] as f64 / 1e3,
    long.len(),
    on_cpu
  );
}
