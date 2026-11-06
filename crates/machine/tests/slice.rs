//! A shard thread's scheduling slice (§4.3, A-224): `slates_machine::probes::request_slice` asks the fair scheduler for
//! a slice, keeping the thread's policy and nice, and reads back what the kernel holds. On Linux 6.12 and later it is
//! held, with no privilege; on an older kernel it is ignored or refused and nothing else about the thread changes; on
//! macOS and Windows nothing is asked. The test prints the process's effective capabilities and the kernel release, so
//! a run as an unprivileged user (a capped `systemd-run --user` unit, CapEff 0) is its own record.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_machine::probes::{Slice, request_slice};
use slates_machine::profile::KERNEL_MIN_SLICE_NS;

/// Shape: the nice the test thread takes before it asks: above the default, so a request that wrote the default nice
/// back would be a raise, which an unprivileged thread is refused (`EPERM`, sched_setattr(2)).
#[cfg(target_os = "linux")]
const RAISED_NICE: i32 = 5;

/// The kernel release's major and minor, from `uname`.
#[cfg(target_os = "linux")]
fn kernel_release() -> (u32, u32) {
  let uname = rustix::system::uname();
  let release = uname.release().to_string_lossy().into_owned();
  let mut parts = release
    .split(|c: char| !c.is_ascii_digit())
    .filter(|part| !part.is_empty());
  let major = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
  let minor = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
  println!("kernel release {release}");
  (major, minor)
}

/// The calling thread's policy and nice, as the OS holds them.
#[cfg(target_os = "linux")]
fn policy_and_nice() -> (i32, i32) {
  // SAFETY: `sched_getscheduler(0)` reads the calling thread's policy and takes no memory.
  let policy = unsafe { libc::sched_getscheduler(0) };
  // SAFETY: `getpriority(PRIO_PROCESS, 0)` reads the calling thread's nice (Linux's per-thread nice) and takes no
  // memory; errno is cleared first since -1 is a valid nice.
  let nice = unsafe {
    *libc::__errno_location() = 0;
    libc::getpriority(libc::PRIO_PROCESS, 0)
  };
  (policy, nice)
}

/// A-224 (§4.3): do raise a fresh thread's nice to at least [`RAISED_NICE`], then ask for the kernel's smallest slice; expect, on
/// Linux 6.12 or later, the slice held as asked with no privilege, and on an older kernel the request ignored or refused;
/// on either, the thread's policy and nice as they were. Off Linux, expect nothing asked.
#[test]
fn a_shard_slice_is_held_without_privilege_and_changes_nothing_else() {
  #[cfg(target_os = "linux")]
  {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let capabilities = status
      .lines()
      .find(|line| line.starts_with("CapEff:"))
      .unwrap_or("CapEff: unknown");
    println!("{capabilities}");
    let (major, minor) = kernel_release();
    std::thread::spawn(move || {
      // Never below the nice the test runs under (a capped unit's `Nice=19`), which would be a raise of priority.
      let nice = policy_and_nice().1.max(RAISED_NICE);
      // SAFETY: `setpriority(PRIO_PROCESS, 0, n)` sets the calling thread's nice and takes no memory; a nice no lower
      // than the thread's own needs no privilege.
      let raised = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
      assert_eq!(raised, 0, "the thread's nice could be raised");
      let before = policy_and_nice();
      let held = request_slice(KERNEL_MIN_SLICE_NS);
      let after = policy_and_nice();
      println!("asked {KERNEL_MIN_SLICE_NS} ns: {held:?}; policy and nice {before:?} -> {after:?}");
      assert_eq!(
        before, after,
        "the request kept the thread's policy and nice"
      );
      assert_eq!(before.1, nice);
      if (major, minor) >= (6, 12) {
        assert_eq!(
          held,
          Slice::Held(KERNEL_MIN_SLICE_NS),
          "Linux 6.12+ holds the slice"
        );
      } else {
        println!(
          "an older kernel: the request is not held ({held:?}), and the thread is unchanged"
        );
        assert!(matches!(held, Slice::Ignored(_) | Slice::Refused(_)));
      }
    })
    .join()
    .unwrap();
  }
  #[cfg(not(target_os = "linux"))]
  assert_eq!(request_slice(KERNEL_MIN_SLICE_NS), Slice::Unsupported);
}
