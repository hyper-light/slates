//! A fair thread may ask EEVDF for its own slice with no privilege (84a's Linux arm E; `slates_machine::fair_slice`).
//! Off Linux the slice is Linux's and the test skips loudly.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use slates_machine::fair_slice::{current, set_current};

/// Shape: a slice far under any kernel's default, R9's provisioning floor.
const ASKED_NS: u64 = 50_000;

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
