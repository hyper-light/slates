//! Where each (transport × suite) cell can run at all, and what stands in its way (§4.6 status
//! blockquotes, GAP-A9-5/GAP-A9-15). This is the table the harness's `plan` step consults before
//! it touches a host: a cell is runnable on a named operating system with named tools (some as a
//! `LIMITED` adapter), or it is owed to a product piece that does not exist yet, or the suite does
//! not apply to the transport. Every reason is a sentence a reader can act on, and the table is
//! the one place those sentences live, so the matrix never says "skipped" without saying why.
//!
//! The facts behind the rows, as of 2026-09-14: the macOS mount is the daemon's NFSv3 loopback
//! server under `mount_nfs` (§4.6, proven live by `crates/cli/tests/cli.rs`); the Linux FUSE
//! bridge (`crates/bridge-fuse`) has its codec, dispatch, `channel::serve_blocking` and
//! `mount::mount`, but no daemon transport serves it (`crates/server` links the crate only as a
//! dev-dependency) and `slates mount` is `mount_nfs`-only, so a Linux run reaches the daemon's
//! NFS export through a root `mount -t nfs` by the OS client — an adapter, recorded `LIMITED`; the
//! Windows WinFsp host is proven live by `crates/bridge-winfsp/tests/mount.rs` (create, write,
//! read, list, delete) and nothing more; virtio-fs is proven only by the simulated guest driver
//! (`docs/wip/virtiofs.md`); the OCI handoff is under construction.

use crate::record::{Suite, Transport};

/// A host operating system a lane runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostOs {
  /// macOS.
  Macos,
  /// Linux.
  Linux,
  /// Windows.
  Windows,
}

impl HostOs {
  /// The lane's name, as the skip reason prints it.
  pub fn lane(self) -> &'static str {
    match self {
      HostOs::Macos => "macOS",
      HostOs::Linux => "Linux",
      HostOs::Windows => "Windows",
    }
  }
}

/// What root buys a suite on a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootNeed {
  /// Root is not needed.
  None,
  /// The suite runs without root at reduced scope (recorded `LIMITED` with this reason).
  ReducesScope(&'static str),
  /// The suite cannot run without root (recorded `SKIPPED(privilege)` with this reason).
  Required(&'static str),
}

/// An adapter that stands in for the offered transport on a lane (recorded `LIMITED`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adapter {
  /// What ran instead of the offered transport.
  pub name: &'static str,
  /// What the adapter does not cover.
  pub not_covered: &'static str,
}

/// Whether a cell can run, and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
  /// Runnable on one operating system with these tools on the `PATH`.
  Runnable {
    /// The lane.
    on: HostOs,
    /// Tools the suite needs on the `PATH` (the harness probes each; a missing one is a skip).
    tools: &'static [&'static str],
    /// What root buys.
    root: RootNeed,
    /// The adapter the lane must use, if the offered transport cannot be driven there.
    adapter: Option<Adapter>,
  },
  /// Not runnable anywhere until the named product piece exists.
  Owed(&'static str),
  /// The suite does not apply to the transport.
  NotApplicable(&'static str),
}

/// The reason no pressure or failure suite can run yet, on any transport.
const NO_PRESSURE_OR_FAILURE_SUITE: &str = concat!(
  "no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real ",
  "processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet"
);

/// The reason every virtio-fs cell is owed.
const VIRTIOFS_OWED: &str = concat!(
  "no live Linux guest: the device half is proven only by the simulated guest driver ",
  "(docs/wip/virtiofs.md), and AC-9.7 says a simulation cannot close the transport guarantee"
);

/// The reason every OCI cell is owed.
const OCI_OWED: &str = "the OCI handoff (the attach verb's container form, crates/server) is under \
  construction; there is no container attachment form to drive a suite through";

/// The Linux adapter: the OS NFS client mounting the unprivileged daemon's loopback export.
const LINUX_NFS_ADAPTER: Adapter = Adapter {
  name: "a root `mount -t nfs` by the Linux NFS client of the unprivileged daemon's NFSv3 loopback \
    export (the same serving code as macOS)",
  not_covered: "the FUSE bridge itself (crates/bridge-fuse): no daemon transport serves /dev/fuse \
    (`crates/server` links the crate only as a dev-dependency) and `slates mount` is `mount_nfs`-only",
};

/// Tools every mounted macOS suite needs.
const MACOS_MOUNT_TOOLS: &[&str] = &["mount_nfs", "umount", "mktemp", "sh"];
/// Tools every mounted Linux suite needs (root mounts through the OS client).
const LINUX_MOUNT_TOOLS: &[&str] = &["mount", "umount", "mktemp", "sh", "sudo"];

/// The availability of a cell.
pub fn availability(transport: Transport, suite: Suite) -> Availability {
  match (transport, suite) {
    (_, Suite::Pressure | Suite::Failure) => Availability::Owed(NO_PRESSURE_OR_FAILURE_SUITE),
    (Transport::VirtioFs, _) => Availability::Owed(VIRTIOFS_OWED),
    (Transport::Oci, _) => Availability::Owed(OCI_OWED),
    (Transport::NativeMacosNfs, suite) => native_macos(suite),
    (Transport::NativeLinuxFuse, suite) => native_linux(suite),
    (Transport::NativeWindowsWinfsp, suite) => native_windows(suite),
  }
}

fn native_macos(suite: Suite) -> Availability {
  let on = HostOs::Macos;
  match suite {
    Suite::Pjdfstest => Availability::Runnable {
      on,
      tools: &["cc", "sh", "mount_nfs", "umount", "mktemp", "openssl", "dd"],
      root: RootNeed::ReducesScope(
        "pjdfstest's README requires root; without it every case that switches uid/gid (`-u`/`-g`) \
         is counted as needs-root, not as a failure",
      ),
      adapter: None,
    },
    Suite::Fsx | Suite::Fsstress => Availability::Runnable {
      on,
      tools: &["cc", "curl", "mount_nfs", "umount", "mktemp", "sh"],
      root: RootNeed::None,
      adapter: None,
    },
    Suite::Workloads => Availability::Runnable {
      on,
      tools: MACOS_MOUNT_TOOLS,
      root: RootNeed::None,
      adapter: None,
    },
    Suite::Hermeticity => Availability::Runnable {
      on,
      tools: &["fs_usage", "sudo", "mount_nfs", "umount", "mktemp", "sh"],
      root: RootNeed::Required(
        "fs_usage needs root for the kernel tracing facility it uses (its manual); macOS has no \
         unprivileged filesystem-write tracer",
      ),
      adapter: None,
    },
    Suite::Pressure | Suite::Failure => Availability::Owed(NO_PRESSURE_OR_FAILURE_SUITE),
  }
}

fn native_linux(suite: Suite) -> Availability {
  let on = HostOs::Linux;
  let adapter = Some(LINUX_NFS_ADAPTER);
  match suite {
    Suite::Pjdfstest => Availability::Runnable {
      on,
      tools: &[
        "cc", "sh", "mount", "umount", "mktemp", "sudo", "openssl", "dd",
      ],
      root: RootNeed::Required(
        "the Linux NFS client mount needs root, and pjdfstest's README requires it",
      ),
      adapter,
    },
    Suite::Fsx | Suite::Fsstress => Availability::Runnable {
      on,
      tools: &["cc", "curl", "mount", "umount", "mktemp", "sh", "sudo"],
      root: RootNeed::Required("the Linux NFS client mount needs root"),
      adapter,
    },
    Suite::Workloads => Availability::Runnable {
      on,
      tools: LINUX_MOUNT_TOOLS,
      root: RootNeed::Required("the Linux NFS client mount needs root"),
      adapter,
    },
    Suite::Hermeticity => Availability::Runnable {
      on,
      tools: &["strace", "mount", "umount", "mktemp", "sh", "sudo"],
      root: RootNeed::Required(
        "the Linux NFS client mount needs root (strace of the harness's own children needs none)",
      ),
      adapter,
    },
    Suite::Pressure | Suite::Failure => Availability::Owed(NO_PRESSURE_OR_FAILURE_SUITE),
  }
}

fn native_windows(suite: Suite) -> Availability {
  match suite {
    Suite::Pjdfstest | Suite::Fsstress => Availability::NotApplicable(
      "a POSIX C suite with no Windows build (pjdfstest and fsstress use fork, uid switching and \
       POSIX namespace calls)",
    ),
    Suite::Fsx => Availability::Owed(
      "the WinFsp fsx port (Phase 4 task 5) is not wired into the harness, and the harness has no \
       WinFsp mount step",
    ),
    Suite::Workloads => Availability::Owed(
      "the harness has no WinFsp mount step; the live WinFsp mount test \
       (crates/bridge-winfsp/tests/mount.rs, gated WINFSP_TEST_MOUNT=1 on windows-latest) proves \
       create/write/read/list/delete through the kernel and nothing more",
    ),
    Suite::Hermeticity => Availability::Owed("no ETW filesystem-write tracer is wired for Windows"),
    Suite::Pressure | Suite::Failure => Availability::Owed(NO_PRESSURE_OR_FAILURE_SUITE),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Every cell has an availability, every owed or inapplicable cell carries a non-empty
  /// reason, and every runnable cell names at least one tool.
  #[test]
  fn every_cell_is_classified_with_a_reason() {
    for transport in Transport::ALL {
      for suite in Suite::ALL {
        match availability(transport, suite) {
          Availability::Runnable { tools, .. } => {
            assert!(!tools.is_empty(), "{transport:?} {suite:?}")
          }
          Availability::Owed(reason) | Availability::NotApplicable(reason) => {
            assert!(!reason.trim().is_empty(), "{transport:?} {suite:?}");
          }
        }
      }
    }
  }

  /// The macOS transport runs its suites on macOS with no adapter; the Linux transport runs them
  /// through the root NFS adapter, which names what it does not cover.
  #[test]
  fn native_lanes_run_on_their_own_os() {
    for suite in [
      Suite::Pjdfstest,
      Suite::Fsx,
      Suite::Fsstress,
      Suite::Workloads,
      Suite::Hermeticity,
    ] {
      match availability(Transport::NativeMacosNfs, suite) {
        Availability::Runnable { on, adapter, .. } => {
          assert_eq!(on, HostOs::Macos);
          assert!(adapter.is_none());
        }
        other => panic!("{suite:?} on macOS: {other:?}"),
      }
      match availability(Transport::NativeLinuxFuse, suite) {
        Availability::Runnable { on, adapter, .. } => {
          assert_eq!(on, HostOs::Linux);
          assert!(adapter.is_some_and(|a| a.not_covered.contains("FUSE")));
        }
        other => panic!("{suite:?} on Linux: {other:?}"),
      }
    }
  }

  /// Guest and container transports are owed for every suite.
  #[test]
  fn guest_and_container_transports_are_owed() {
    for suite in Suite::ALL {
      assert!(matches!(
        availability(Transport::VirtioFs, suite),
        Availability::Owed(_)
      ));
      assert!(matches!(
        availability(Transport::Oci, suite),
        Availability::Owed(_)
      ));
    }
  }

  /// The pressure and failure columns are owed on every transport.
  #[test]
  fn pressure_and_failure_suites_are_owed() {
    for transport in Transport::ALL {
      assert!(matches!(
        availability(transport, Suite::Pressure),
        Availability::Owed(_)
      ));
      assert!(matches!(
        availability(transport, Suite::Failure),
        Availability::Owed(_)
      ));
    }
  }
}
