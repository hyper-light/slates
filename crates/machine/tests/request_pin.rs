//! A client thread's synchronous request runs on its shard's CPU and its own CPU mask is put back exactly after it (84a's
//! Linux arm F; `slates_machine::request_pin`). Off Linux nothing is pinned and the tests skip loudly.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(target_os = "linux")]
use slates_machine::request_pin::current_cpu;
use slates_machine::request_pin::{PinSkipped, pin_for_request};

/// The CPUs in the calling thread's own mask, in order.
#[cfg(target_os = "linux")]
fn own_cpus() -> Vec<usize> {
  let own = rustix::thread::sched_getaffinity(None).unwrap();
  (0..rustix::thread::CpuSet::MAX_CPU)
    .filter(|cpu| own.is_set(*cpu))
    .collect()
}

/// 84a arm F, Linux. Do: on a fresh thread, pin a request to the last CPU of the thread's own mask, then restore.
/// Expect: the thread runs on that CPU while pinned, and its mask afterwards is the one it had before, CPU for CPU.
#[test]
fn a_pinned_request_runs_on_its_cpu_and_its_own_mask_comes_back() {
  #[cfg(target_os = "linux")]
  std::thread::spawn(|| {
    let before = own_cpus();
    let target = *before.last().unwrap();
    let pin =
      pin_for_request(target).expect("a thread pins itself to a CPU of its own mask unprivileged");
    // The pin migrates the thread before the call returns; a yield lets a busy runner's scheduler catch up.
    std::thread::yield_now();
    assert_eq!(
      current_cpu(),
      Some(target),
      "the pinned request runs on its CPU"
    );
    assert_eq!(own_cpus(), vec![target]);
    pin.restore().expect("a thread puts its own mask back");
    assert_eq!(own_cpus(), before, "the thread's own mask, CPU for CPU");
  })
  .join()
  .unwrap();
  #[cfg(not(target_os = "linux"))]
  eprintln!("skipping: a request's CPU pin is Linux's");
}

/// 84a arm F, Linux. Do: narrow a fresh thread's mask to one CPU, then pin a request to another. Expect: refused as
/// outside the thread's mask, and the thread's mask unchanged (a caller's narrowing is never widened).
#[test]
fn a_request_never_pins_outside_the_threads_own_mask() {
  #[cfg(target_os = "linux")]
  std::thread::spawn(|| {
    let cpus = own_cpus();
    if cpus.len() < 2 {
      eprintln!("skipping: one CPU in this thread's mask, none outside it to refuse");
      return;
    }
    let first = *cpus.first().unwrap();
    let outside = *cpus.last().unwrap();
    let narrowed = pin_for_request(first).unwrap();
    assert_eq!(
      pin_for_request(outside).err(),
      Some(PinSkipped::OutsideMask)
    );
    assert_eq!(
      own_cpus(),
      vec![first],
      "the refused pin left the mask as it was"
    );
    narrowed.restore().unwrap();
    assert_eq!(own_cpus(), cpus);
  })
  .join()
  .unwrap();
  #[cfg(not(target_os = "linux"))]
  assert_eq!(pin_for_request(0).err(), Some(PinSkipped::Unsupported));
}
