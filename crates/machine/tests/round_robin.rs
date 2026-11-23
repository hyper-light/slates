//! A client's reply wait parks in Linux's round-robin real-time class where the account's limits grant it, and its
//! thread's own scheduling is put back exactly after the park (84a's Linux half; `slates_machine::round_robin`): the
//! caller's code never runs at real-time priority. A thread already in a real-time class keeps its own, and one the
//! limits deny stays as it is. Off Linux the class is Linux's and the tests skip loudly.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use slates_machine::round_robin::{
  WaitSkipped, granted_priority, holds, raise_for_wait, take_current,
};

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

/// 84a, Linux. Do: on a fresh time-share thread, raise its nice by one, then raise it for a park and restore it. Expect,
/// where the account's limits grant the class: in the class during the park, then time-share again with the nice it had;
/// where they grant none, the raise declines and the thread is untouched.
#[test]
fn a_parks_real_time_class_is_put_back_exactly_after_it() {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping: the round-robin real-time class is Linux's");
    return;
  }
  #[cfg(target_os = "linux")]
  std::thread::spawn(|| {
    let own = nice().saturating_add(1);
    set_nice(own);
    if granted_priority().is_err() {
      eprintln!("this account grants no real-time class: the park must stay as it is");
      assert!(matches!(raise_for_wait(), Err(WaitSkipped::Declined(_))));
      assert_eq!((holds(), nice()), (Some(false), own));
      return;
    }
    let class = raise_for_wait().expect("a granted park takes the class");
    assert_eq!(holds(), Some(true), "in the class during the park");
    class.restore().expect("the thread's own class put back");
    assert_eq!(
      (holds(), nice()),
      (Some(false), own),
      "time-share again, its own nice kept"
    );
  })
  .join()
  .unwrap();
}

/// 84a, Linux. Do: on a thread already in the round-robin class (where granted), raise it for a park. Expect: skipped,
/// `AlreadyRealTime`, its class kept.
#[test]
fn a_thread_already_real_time_keeps_its_own_class() {
  if !cfg!(target_os = "linux") || granted_priority().is_err() {
    eprintln!("skipping: needs Linux and a granted real-time class");
    return;
  }
  std::thread::spawn(|| {
    take_current().expect("a granted thread takes the class");
    assert_eq!(
      raise_for_wait().map(|_| ()),
      Err(WaitSkipped::AlreadyRealTime)
    );
    assert_eq!(holds(), Some(true), "its own class kept");
  })
  .join()
  .unwrap();
}
