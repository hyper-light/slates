//! A fair thread may ask EEVDF for its own slice with no privilege, and a client's park's asked slice is put back exactly
//! after it (84a's Linux arm E; `slates_machine::fair_slice`). Off Linux the slice is Linux's and the tests skip loudly.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use slates_machine::fair_slice::{current, raise_for_park, set_current};

/// Shape: a slice far under any kernel's default, R9's provisioning floor.
const ASKED_NS: u64 = 50_000;

/// The calling thread's nice value (Linux keeps one a thread).
#[cfg(target_os = "linux")]
fn nice() -> i32 {
  // SAFETY: a query of the calling thread's own nice, by its thread id; errno is cleared first since -1 is a value.
  unsafe {
    *libc::__errno_location() = 0;
    let tid = libc::syscall(libc::SYS_gettid);
    libc::getpriority(libc::PRIO_PROCESS, u32::try_from(tid).unwrap())
  }
}

/// Sets the calling thread's nice value (raising it needs no privilege).
#[cfg(target_os = "linux")]
fn set_nice(value: i32) {
  // SAFETY: the calling thread's own nice, by its thread id.
  let rc = unsafe {
    let tid = libc::syscall(libc::SYS_gettid);
    libc::setpriority(libc::PRIO_PROCESS, u32::try_from(tid).unwrap(), value)
  };
  assert_eq!(rc, 0, "a thread raises its own nice");
}

/// 84a arm E, Linux. Do: on a fresh thread, ask a slice of [`ASKED_NS`]. Expect: the slice the kernel reports back is
/// shorter than the thread's default, and no shorter than asked (the kernel raises it to its own least).
#[test]
fn a_fair_thread_holds_the_slice_it_asks_with_no_privilege() {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping: EEVDF's requested slice is Linux's");
    return;
  }
  std::thread::spawn(|| {
    let before = current().expect("a thread's slice is readable");
    let held = set_current(ASKED_NS).expect("a fair thread asks its slice unprivileged");
    assert!(
      held >= ASKED_NS && held < before,
      "held {held} ns against {before} ns before"
    );
    assert_eq!(current(), Some(held));
  })
  .join()
  .unwrap();
}

/// 84a arm E, Linux. Do: on a fresh thread, raise its nice by one, then ask a park's slice and restore it. Expect: the
/// asked slice during the park, then the thread's own slice and nice again.
#[test]
fn a_parks_slice_is_put_back_exactly_after_it() {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping: EEVDF's requested slice is Linux's");
    return;
  }
  #[cfg(target_os = "linux")]
  std::thread::spawn(|| {
    let own_nice = nice().saturating_add(1);
    set_nice(own_nice);
    let before = current().expect("a thread's slice is readable");
    let slice = raise_for_park(ASKED_NS).expect("a fair thread asks its park's slice");
    assert!(
      current().is_some_and(|during| during < before),
      "the park's slice held"
    );
    slice
      .restore()
      .expect("the thread's own scheduling put back");
    assert_eq!(
      (current(), nice()),
      (Some(before), own_nice),
      "its own slice and nice again"
    );
  })
  .join()
  .unwrap();
}
