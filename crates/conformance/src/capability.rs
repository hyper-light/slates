//! Where each (transport × suite) cell can run at all, and what stands in its way (§4.6 status
//! blockquotes, GAP-A9-5/GAP-A9-15). This is the table the harness's `plan` step consults before
//! it touches a host: a cell is runnable on a named operating system with named tools (some as a
//! `LIMITED` adapter), or it is owed to a product piece that does not exist yet, or the suite does
//! not apply to the transport. Every reason is a sentence a reader can act on, and the table is
//! the one place those sentences live, so the matrix never says "skipped" without saying why.
//!
//! The facts behind the rows, as of 2026-10-01: the macOS mount is the daemon's NFSv3 loopback
//! server under `mount_nfs` (§4.6, proven live by `crates/cli/tests/cli.rs`); the daemon serves a Linux
//! FUSE mount (`slates mount` on Linux, `crates/server/src/fuse.rs`, proven on a real kernel mount by
//! `crates/server/tests/fuse_mount.rs`), but the Linux lane still reaches the daemon through a root
//! `mount -t nfs` adapter — running the suites over the FUSE mount itself is owed (pjdfstest's root cases
//! need `allow_other`, an operator's `user_allow_other`); the Windows WinFsp host is proven live by
//! `crates/bridge-winfsp/tests/mount.rs` (create, write, read, list, delete) and nothing more;
//! virtio-fs runs a live Linux guest through QEMU's vhost-user-fs, which mounts the tag and runs the workload
//! roster identically on slates and on its RAM (`crates/server/tests/virtiofs.rs`), while the other suites have
//! no guest leg yet; the OCI container
//! form exists (`attach` with the container form: a verified non-recursive private bind and `slates
//! oci-check`) and a workload ran through it on Docker Desktop (T-4.13); fsx runs inside a container through
//! it on the macOS lane (`xtask/src/conformance/container.rs`), as do fsstress, the workloads and pjdfstest;
//! the hermeticity container leg is owed.

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

/// The reason the virtio-fs workloads cell is not run by this harness: it ran in a live guest elsewhere.
const VIRTIOFS_WORKLOADS_IN_THE_GUEST_TEST: &str = concat!(
  "the roster runs in a live Linux guest through QEMU's vhost-user-fs-pci, each workload compared between ",
  "slates and the guest's RAM — all nine identical on 2026-10-01 (crates/server/tests/virtiofs.rs ",
  "a_live_guest_runs_the_roster_workloads_identically_on_slates_and_on_its_ram, gated on QEMU and a guest ",
  "kernel); this harness has no VMM leg (a vhost-user device is attached in-process), so the cell is not run here"
);

/// The reason every other virtio-fs cell is owed.
const VIRTIOFS_OWED: &str = concat!(
  "a live Linux guest mounts the tag through QEMU's vhost-user-fs-pci and runs the workload roster ",
  "(crates/server/tests/virtiofs.rs), but this suite has no guest leg yet; AC-9.7 asks it run in the guest"
);

/// The reason every OCI cell is owed.
const OCI_OWED: &str = "the container form exists — `attach` returns a verified non-recursive private bind \
  (the source checked again by `slates oci-check`), and fsx runs inside a container through it on the macOS \
  lane under the runtime handshake (`slates oci-runtime docker`), as do fsstress, the workloads and \
  pjdfstest — but the hermeticity container leg is not built: its tracer needs root, and no lane holds both \
  root and a container engine; on Linux the bind is refused ContainerWorkloadUnproven until a workload runs through the \
  daemon's FUSE mount";

/// The Linux adapter: the OS NFS client mounting the unprivileged daemon's loopback export.
const LINUX_NFS_ADAPTER: Adapter = Adapter {
  name: "a root `mount -t nfs` by the Linux NFS client of the unprivileged daemon's NFSv3 loopback \
    export (the same serving code as macOS)",
  not_covered: "the daemon's own FUSE mount (`slates mount` on Linux, crates/server/src/fuse.rs): the lane \
    still mounts through the NFS adapter; running the suites over FUSE is owed (pjdfstest's root cases need \
    `allow_other`, which an operator grants with `user_allow_other` in /etc/fuse.conf)",
};

/// Tools every mounted macOS suite needs.
const MACOS_MOUNT_TOOLS: &[&str] = &["mount_nfs", "umount", "sh"];
/// Tools every mounted Linux suite needs (root mounts through the OS client).
const LINUX_MOUNT_TOOLS: &[&str] = &["mount", "umount", "sh", "sudo"];

/// The availability of a cell.
pub fn availability(transport: Transport, suite: Suite) -> Availability {
  match (transport, suite) {
    (_, Suite::Pressure | Suite::Failure) => Availability::Owed(NO_PRESSURE_OR_FAILURE_SUITE),
    (Transport::VirtioFs, Suite::Workloads) => {
      Availability::Owed(VIRTIOFS_WORKLOADS_IN_THE_GUEST_TEST)
    }
    (Transport::VirtioFs, _) => Availability::Owed(VIRTIOFS_OWED),
    // fsx compiled and run inside a container over the exact entry `attach --oci` returns (AUD-29-78).
    (Transport::Oci, Suite::Fsx | Suite::Fsstress | Suite::Workloads | Suite::Pjdfstest) => {
      Availability::Runnable {
        on: HostOs::Macos,
        tools: &["mount_nfs", "umount", "sh", "cc", "docker"],
        root: RootNeed::None,
        adapter: None,
      }
    }
    (Transport::Oci, _) => Availability::Owed(OCI_OWED),
    (Transport::NativeMacosNfs, suite) => native_macos(suite),
    (Transport::NativeLinuxFuse, suite) => native_linux(suite, Some(LINUX_NFS_ADAPTER)),
    // The daemon's own NFSv4.2 server under the kernel client: a transport, not an adapter.
    (Transport::NativeLinuxNfs4, suite) => native_linux(suite, None),
    (Transport::NativeWindowsWinfsp, suite) => native_windows(suite),
  }
}

fn native_macos(suite: Suite) -> Availability {
  let on = HostOs::Macos;
  match suite {
    Suite::Pjdfstest => Availability::Runnable {
      on,
      tools: &["cc", "sh", "mount_nfs", "umount", "openssl", "dd"],
      root: RootNeed::ReducesScope(
        "pjdfstest's README requires root; without it every case that switches uid/gid (`-u`/`-g`) \
         is counted as needs-root, not as a failure",
      ),
      adapter: None,
    },
    Suite::Fsx | Suite::Fsstress => Availability::Runnable {
      on,
      tools: &["cc", "curl", "mount_nfs", "umount", "sh"],
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
      tools: &["fs_usage", "sudo", "mount_nfs", "umount", "sh"],
      root: RootNeed::Required(
        "fs_usage needs root for the kernel tracing facility it uses (its manual); macOS has no \
         unprivileged filesystem-write tracer",
      ),
      adapter: None,
    },
    Suite::Pressure | Suite::Failure => Availability::Owed(NO_PRESSURE_OR_FAILURE_SUITE),
  }
}

fn native_linux(suite: Suite, adapter: Option<Adapter>) -> Availability {
  let on = HostOs::Linux;
  match suite {
    Suite::Pjdfstest => Availability::Runnable {
      on,
      tools: &["cc", "sh", "mount", "umount", "sudo", "openssl", "dd"],
      root: RootNeed::Required(
        "the Linux NFS client mount needs root, and pjdfstest's README requires it",
      ),
      adapter,
    },
    Suite::Fsx | Suite::Fsstress => Availability::Runnable {
      on,
      tools: &["cc", "curl", "mount", "umount", "sh", "sudo"],
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
      tools: &["strace", "mount", "umount", "sh", "sudo"],
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

  /// The guest transport is owed for every suite; the container transport runs fsx on the macOS lane, inside a
  /// container over the OCI bind (AUD-29-78), and is owed for every other suite.
  #[test]
  fn the_guest_is_owed_and_the_container_runs_only_its_built_leg() {
    for suite in Suite::ALL {
      assert!(matches!(
        availability(Transport::VirtioFs, suite),
        Availability::Owed(_)
      ));
      assert_container_cell(suite);
    }
  }

  /// The container cell for `suite`: runnable on macOS with `docker` where its leg is built, else owed.
  fn assert_container_cell(suite: Suite) {
    let container = availability(Transport::Oci, suite);
    let built = matches!(
      suite,
      Suite::Fsx | Suite::Fsstress | Suite::Workloads | Suite::Pjdfstest
    );
    let runnable = matches!(container, Availability::Runnable { on: HostOs::Macos, tools, adapter: None, .. } if tools.contains(&"docker"));
    let owed = matches!(container, Availability::Owed(_));
    assert!(
      if built { runnable } else { owed },
      "{suite:?}: {container:?}"
    );
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
