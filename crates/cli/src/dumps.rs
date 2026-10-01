//! Dump exclusion for the processes that hold private bytes (AUD-29-41 in part; §4.2 "RAM-only guarantees",
//! R1): the anchor (the segment and the recovery images) and the daemon (every volume) exclude themselves
//! from core dumps before they map or receive any private byte, and refuse to start if they cannot. A core
//! dump is the kernel writing a process's memory to disk or to a crash collector; nothing here asks it
//! to, and this module makes sure it cannot carry slates' memory when the process faults.
//!
//! Two settings, each closing a different path:
//! - **The core size limit is 0, soft and hard** (`setrlimit(RLIMIT_CORE)`, every Unix): the kernel writes no
//!   core file for the process, and the process can never raise the limit again. Lowering a limit needs no
//!   privilege (R10). On macOS this is the whole mechanism (`kern.coredump` writes to `/cores` only within the
//!   limit); crash reports carry registers and stacks' return addresses, not heap.
//! - **The core filter is empty** (`/proc/self/coredump_filter` = 0, Linux): a pipe collector (`apport`,
//!   `systemd-coredump`) is not bound by the size limit; with no mapping class selected, its dump carries no
//!   anonymous, shared or file-backed memory — not the heap, not the stacks, not the shared segment — only
//!   registers and the vDSO.
//!
//! Both survive `fork` and `exec`, so the daemon the anchor spawns starts excluded and then sets both itself.
//! Residency (no pageout) is the other half of AUD-29-41 and is not established here.
//!
//! Measured and set aside (2026-10-01): making the processes not dumpable (`prctl(PR_SET_DUMPABLE, 0)`) as
//! well. It adds no dump content the two settings above leave, but it also closes the processes' `/proc` to
//! every other process of the same user — the path the conformance harness reaches the anchor segment's
//! descriptor by to issue a grant on Linux, where the issuer surface is owed (§4.13, `docs/wip/enrollment.md`).
//! Who may reach the segment is that surface's decision, so the flag waits for it (and, set, it must come
//! after the filter: a non-dumpable process's `/proc/self` files belong to root, and an ordinary user's write
//! to the filter is then refused — found under the harness's non-root user).

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

/// The Linux setting: an empty core filter.
#[cfg(target_os = "linux")]
fn exclude_linux_paths() -> Result<(), Failure> {
  use rustix::fs::{Mode, OFlags};
  /// Format: the core filter that selects no mapping class (`core(5)`, "Controlling which mappings are
  /// written to the core dump"), as the hexadecimal text the file reads.
  const NO_MAPPING_CLASSES: &[u8] = b"0";
  /// Format: the per-process core filter, a kernel control file (procfs), not a disk file.
  const CORE_FILTER: &str = "/proc/self/coredump_filter";
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
  /// back. Expect: the limit reads 0 soft and hard and cannot be raised; on Linux its core filter reads 0.
  /// (The real anchor and daemon are observed from outside in `tests/cli.rs`.)
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
      let filter = std::fs::read_to_string("/proc/self/coredump_filter").unwrap();
      assert_eq!(u32::from_str_radix(filter.trim(), 16).unwrap(), 0);
    }
  }
}
