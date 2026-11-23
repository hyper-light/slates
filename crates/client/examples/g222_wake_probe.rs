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
  let settle_ns: u64 = std::env::var("PROBE_SETTLE_NS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(50_000);
  let jitter_ns: u64 = std::env::var("PROBE_JITTER_NS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(0);
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
  // Words and acknowledgements. By default one shared anonymous mapping, a page apart. With `PROBE_SHM=1`, as the
  // daemon's client regions are: each waiter's own `shm_open` object of `PROBE_REGION_MB` (default 8), mapped twice,
  // the waiter waiting through one mapping and the waker writing, waking and reading the acknowledgement through the
  // other; `PROBE_WRITE_KB` bytes are written into the waker's mapping before each wake, as a reply's body is.
  let page = 16_384usize;
  let shm = flag("PROBE_SHM");
  let region: usize = std::env::var("PROBE_REGION_MB")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(8)
    << 20;
  let write_bytes: usize = std::env::var("PROBE_WRITE_KB")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(0)
    << 10;
  let map = |fd: i32, len: usize| -> usize {
    // SAFETY: a shared mapping of the stated length, of an anonymous region or of `fd`; checked below.
    let at = unsafe {
      libc::mmap(
        std::ptr::null_mut(),
        len,
        libc::PROT_READ | libc::PROT_WRITE,
        if fd < 0 {
          libc::MAP_SHARED | libc::MAP_ANON
        } else {
          libc::MAP_SHARED
        },
        fd,
        0,
      )
    };
    assert!(at != libc::MAP_FAILED, "mmap");
    at as usize
  };
  let (waiter_maps, waker_maps): (Vec<usize>, Vec<usize>) = if shm {
    (0..waiters)
      .map(|i| {
        let name = std::ffi::CString::new(format!("/g222p{}-{i}", std::process::id())).unwrap();
        // SAFETY: a NUL-terminated name; the object is unlinked at once, its mappings keep it.
        let fd = unsafe {
          libc::shm_open(
            name.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
            0o600,
          )
        };
        assert!(fd >= 0, "shm_open");
        // SAFETY: the object just opened.
        unsafe {
          assert_eq!(libc::ftruncate(fd, region as libc::off_t), 0, "ftruncate");
          libc::shm_unlink(name.as_ptr());
        }
        let pair = (map(fd, region), map(fd, region));
        // SAFETY: the descriptor, no longer needed once mapped.
        unsafe { libc::close(fd) };
        pair
      })
      .unzip()
  } else {
    let base = map(-1, page * waiters * 2);
    let at: Vec<usize> = (0..waiters).map(|i| base + i * 2 * page).collect();
    (at.clone(), at)
  };
  let waiter_maps = &waiter_maps;
  let waker_maps = &waker_maps;
  // SAFETY (all four): inside a live mapping, aligned, zeroed, live for the process.
  let word =
    move |i: usize| -> &'static AtomicU32 { unsafe { &*(waiter_maps[i] as *const AtomicU32) } };
  let ack = move |i: usize| -> &'static AtomicU32 {
    unsafe { &*((waiter_maps[i] + page) as *const AtomicU32) }
  };
  let wword =
    move |i: usize| -> &'static AtomicU32 { unsafe { &*(waker_maps[i] as *const AtomicU32) } };
  let wack = move |i: usize| -> &'static AtomicU32 {
    unsafe { &*((waker_maps[i] + page) as *const AtomicU32) }
  };
  let body = move |i: usize| -> usize { waker_maps[i] + 2 * page };
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
            let next = wword(i).load(Ordering::Relaxed) + 1;
            // Give the waiter time to sleep again, as a client parks between requests: `PROBE_SETTLE_NS` (default 50 µs),
            // or with `PROBE_JITTER_NS` a spread of 0 to that, so wakes meet waiters entering their wait.
            let wait_ns = if jitter_ns > 0 {
              (n as u64).wrapping_mul(2_654_435_761) % jitter_ns
            } else {
              settle_ns
            };
            let settle = mono();
            while mono() - settle < wait_ns {
              std::hint::spin_loop();
            }
            if shm && write_bytes > 0 {
              // SAFETY: inside the waker's mapping of this waiter's region.
              unsafe {
                std::ptr::write_bytes(
                  body(i) as *mut u8,
                  (n & 0xff) as u8,
                  write_bytes.min(region - 2 * page),
                )
              };
            }
            wword(i).store(next, Ordering::Release);
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
                  libc::os_sync_wake_by_address_any(wword(i).as_ptr().cast(), 4, wake_flags);
                }
              }
            }
            out.push((mono() - w0, cpu() - c0));
            while wack(i).load(Ordering::Acquire) != next {
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
    "probe settle_ns={} jitter_ns={} shm={} write_kb={} mech={} churn={} shared={} waker_tb={} waiter_tb={} wakers={} waiters={} spinners/core={}: wake wall p50 {:.1} p99 {:.1} p999 {:.1} max {:.1} us; >=1 ms {} ({} on CPU >=90%)",
    settle_ns,
    jitter_ns,
    u8::from(shm),
    write_bytes >> 10,
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
