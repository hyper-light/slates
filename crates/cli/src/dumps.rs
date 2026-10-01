//! Dump exclusion for the processes that hold private bytes (AUD-29-41 in part; §4.2 "RAM-only guarantees",
//! R1): the anchor (the segment and the recovery images) and the daemon (every volume) exclude themselves
//! from core dumps before they map or receive any private byte, and refuse to start if they cannot. A core
//! dump is the kernel writing a process's memory to disk or to a crash collector; nothing here asks it
//! to, and this module makes sure it cannot carry slates' memory when the process faults.
//!
//! Three settings, each closing a different path:
//! - **The core size limit is 0, soft and hard** (`setrlimit(RLIMIT_CORE)`, every Unix): the kernel writes no
//!   core file for the process, and the process can never raise the limit again. Lowering a limit needs no
//!   privilege (R10). On macOS this is the whole mechanism (`kern.coredump` writes to `/cores` only within the
//!   limit); crash reports carry registers and stacks' return addresses, not heap.
//! - **The process is not dumpable** (`prctl(PR_SET_DUMPABLE, 0)`, Linux): no core at the default
//!   `fs.suid_dumpable=0`, and other processes of the same user — debuggers, `/proc/<pid>/mem`,
//!   `process_vm_readv`, `pidfd_getfd` — cannot read its memory or descriptors. Its own `/proc/self` stays
//!   usable (the kernel exempts the same thread group), which the landing's `linkat` through
//!   `/proc/self/fd` relies on.
//! - **The core filter is empty** (`/proc/self/coredump_filter` = 0, Linux): a pipe collector (`apport`,
//!   `systemd-coredump`) is not bound by the size limit, and `fs.suid_dumpable=2` still dumps a non-dumpable
//!   process to it; with no mapping class selected, such a dump carries no anonymous, shared or file-backed
//!   memory — not the heap, not the stacks, not the shared segment — only registers and the vDSO.
//!
//! The settings survive `fork`; the core limit and the filter survive `exec` as well, so the daemon the
//! anchor spawns starts excluded and then sets all three itself. Residency (no pageout) is the other half of
//! AUD-29-41 and is not established here.

use crate::Failure;

/// Format: the core size limit that writes no core, soft and hard.
const NO_CORE_BYTES: u64 = 0;

/// Excludes this process from core dumps, or refuses with the setting the host would not take.
pub(crate) fn exclude_from_dumps() -> Result<(), Failure> {
  let no_core = rustix::process::Rlimit {
    current: Some(NO_CORE_BYTES),
    maximum: Some(NO_CORE_BYTES),
  };
  rustix::process::setrlimit(rustix::process::Resource::Core, no_core)
    .map_err(|e| refused("the core size limit", e))?;
  exclude_linux_paths()
}

/// The Linux settings: not dumpable, and an empty core filter.
#[cfg(target_os = "linux")]
fn exclude_linux_paths() -> Result<(), Failure> {
  use rustix::fs::{Mode, OFlags};
  /// Format: the core filter that selects no mapping class (`core(5)`, "Controlling which mappings are
  /// written to the core dump"), as the hexadecimal text the file reads.
  const NO_MAPPING_CLASSES: &[u8] = b"0";
  /// Format: the per-process core filter, a kernel control file (procfs), not a disk file.
  const CORE_FILTER: &str = "/proc/self/coredump_filter";
  rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    .map_err(|e| refused("non-dumpable", e))?;
  // structural: allow — a write to a /proc control file of this process, not a disk file (R1).
  let filter = rustix::fs::open(CORE_FILTER, OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty())
    .map_err(|e| refused("the core filter", e))?;
  let written =
    rustix::io::write(&filter, NO_MAPPING_CLASSES).map_err(|e| refused("the core filter", e))?;
  if written != NO_MAPPING_CLASSES.len() {
    return Err(Failure::Failed(format!(
      "cannot exclude this process from core dumps: the core filter took {written} of {} bytes",
      NO_MAPPING_CLASSES.len()
    )));
  }
  Ok(())
}

/// No Linux-only path to close elsewhere: the core limit is the mechanism.
#[cfg(not(target_os = "linux"))]
fn exclude_linux_paths() -> Result<(), Failure> {
  Ok(())
}

fn refused(setting: &str, e: rustix::io::Errno) -> Failure {
  Failure::Failed(format!(
    "cannot exclude this process from core dumps: {setting} was refused (code {})",
    e.raw_os_error()
  ))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// AUD-29-41 (dump exclusion). Do: exclude this test process from dumps, then try to raise its core limit
  /// back. Expect: the limit reads 0 soft and hard and cannot be raised; on Linux the process reads
  /// non-dumpable and its core filter reads 0. (The real anchor and daemon are observed from outside in
  /// `tests/cli.rs`.)
  #[test]
  fn a_process_excludes_itself_from_core_dumps_for_good() {
    exclude_from_dumps().unwrap();
    let limit = rustix::process::getrlimit(rustix::process::Resource::Core);
    assert_eq!((limit.current, limit.maximum), (Some(0), Some(0)));
    let raise = rustix::process::Rlimit {
      current: Some(1),
      maximum: Some(1),
    };
    if rustix::process::geteuid().is_root() {
      eprintln!("SKIP the raise check: root may raise a hard limit (CAP_SYS_RESOURCE)");
    } else {
      assert!(
        rustix::process::setrlimit(rustix::process::Resource::Core, raise).is_err(),
        "an unprivileged process cannot raise its hard limit"
      );
    }
    #[cfg(target_os = "linux")]
    {
      assert_eq!(
        rustix::process::dumpable_behavior().unwrap(),
        rustix::process::DumpableBehavior::NotDumpable
      );
      let filter = std::fs::read_to_string("/proc/self/coredump_filter").unwrap();
      assert_eq!(u32::from_str_radix(filter.trim(), 16).unwrap(), 0);
    }
  }
}
