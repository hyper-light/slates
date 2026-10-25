//! The transport capability report (§4.6 A-9: "Capabilities differ by host, kernel, runtime and
//! VMM and must be reported by `attach` and `status`: supported transport, target-path constraints,
//! read/write policy, sharing/cache semantics, residency boundary and conformance evidence"; AC-9.7:
//! "A skipped lane or pure simulation cannot close its transport guarantee"). Every field states
//! what is true of this build, and the conformance evidence names the simulated driver as the
//! evidence — never a live guest until one has run. DAX is reported as not advertised, with the
//! contract's reason (AC-4.12 "Do not advertise DAX without this gate").

use slates_bridge_core::Rights;
use slates_bridge_fuse::abi::flags;
use slates_bridge_fuse::init::InitNegotiation;

use crate::admission::{GuestTransport, UnsupportedReason};

/// Format: the contract's reason a DAX capability is not advertised (§4.6 A-9; AC-4.12).
pub const DAX_NOT_ADVERTISED_REASON: &str = "the baseline contract does not require DAX; a requested DAX capability cannot be advertised until mapping isolation, pinning and teardown have been established for this VMM (§4.6 A-9, AC-4.12)";
/// Format: why the vhost-user binding offers DAX (§4.6; A-168, A-169): its windows map per-file copies kept equal to the
/// volume, each authorizing one file, so isolation and teardown hold by construction.
pub const DAX_OFFERED_PER_ATTACH_REASON: &str = "a DAX window is offered when the attach names one: its windows map per-file copies slates keeps equal to the volume, each authorizing one file's bytes (§4.6, A-168)";

/// Where the guest finds the volume: it mounts the device's tag (`mount -t virtiofs <tag>`); no
/// host path exists for it, no directory is created, no socket is placed on disk (§4.6: "No disk
/// socket, image construction, target mkdir or privilege escalation is implicit").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetPath {
  /// The tag an attached device publishes.
  GuestTag {
    /// The tag.
    tag: String,
  },
  /// The tag is assigned when a device is attached (the host-level statement of the constraint).
  GuestTagAssignedAtAttach,
}

/// What the attachment's rights allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadWritePolicy {
  /// Reads only; every mutating request is refused.
  ReadOnly,
  /// Reads and writes.
  ReadWrite,
}

/// The sharing and cache semantics the guest sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharingSemantics {
  /// The volume has one owning shard; every request is served in order by one task (D-7).
  pub one_owning_shard: bool,
  /// Whether the guest kernel negotiated writeback caching at INIT (dirty pages held in the guest
  /// until it writes them back through the device).
  pub writeback_cache: bool,
  /// Whether explicit data invalidation was negotiated at INIT.
  pub explicit_invalidation: bool,
}

/// Where the bytes live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Residency {
  /// In the host daemon's RAM (R1); the guest holds page-cache copies it writes back through the
  /// device; nothing on disk.
  HostRam,
}

/// The evidence behind the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conformance {
  /// The simulated guest driver's differential oracle against direct FUSE dispatch (this crate's
  /// tests); no live guest has run.
  SimulatedGuestDriver,
  /// A live Linux guest under a real VMM mounted the tag and ran the conformance roster's workloads, each
  /// identical on the volume and on the guest's own RAM (AC-9.7;
  /// `a_live_guest_runs_the_roster_workloads_identically_on_slates_and_on_its_ram`).
  LiveGuestWorkloads,
}

/// The DAX line of the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DaxCapability {
  /// Whether DAX is advertised to the guest.
  pub advertised: bool,
  /// Why not.
  pub reason: &'static str,
}

/// One transport's capability report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportCapability {
  /// The transport.
  pub transport: GuestTransport,
  /// Whether this build can serve it.
  pub supported: bool,
  /// Why not, when it cannot.
  pub unsupported_reason: Option<UnsupportedReason>,
  /// The target-path constraint.
  pub target_path: TargetPath,
  /// The read/write policy.
  pub read_write: ReadWritePolicy,
  /// The sharing and cache semantics.
  pub sharing: SharingSemantics,
  /// The residency boundary.
  pub residency: Residency,
  /// The conformance evidence.
  pub conformance: Conformance,
  /// DAX.
  pub dax: DaxCapability,
}

/// The sharing semantics from a negotiated FUSE connection (none negotiated: nothing claimed).
fn sharing_of(negotiated: Option<InitNegotiation>) -> SharingSemantics {
  let flags = negotiated.map_or(0, |n| n.flags);
  SharingSemantics {
    one_owning_shard: true,
    writeback_cache: flags & flags::WRITEBACK_CACHE != 0,
    explicit_invalidation: flags & flags::EXPLICIT_INVAL_DATA != 0,
  }
}

/// The DAX line of `transport`: the vhost-user binding offers DAX on Linux, where its windows map per-file backings
/// (A-168); the in-process binding's fd-sharing placer is not established, so it advertises none.
const fn dax(transport: GuestTransport) -> DaxCapability {
  match transport {
    GuestTransport::InheritedDescriptor if cfg!(target_os = "linux") => DaxCapability {
      advertised: true,
      reason: DAX_OFFERED_PER_ATTACH_REASON,
    },
    GuestTransport::InheritedDescriptor | GuestTransport::InProcess => DaxCapability {
      advertised: false,
      reason: DAX_NOT_ADVERTISED_REASON,
    },
  }
}

/// What this build offers for `transport` before any device is attached.
pub fn host_capability(transport: GuestTransport) -> TransportCapability {
  let unsupported_reason = match transport {
    GuestTransport::InProcess => None,
    // vhost-user (`crate::vhost_user`): Linux's eventfds and the KVM VMMs that speak it.
    GuestTransport::InheritedDescriptor if cfg!(target_os = "linux") => None,
    GuestTransport::InheritedDescriptor => Some(UnsupportedReason::BindingNotBuilt),
  };
  TransportCapability {
    transport,
    supported: unsupported_reason.is_none(),
    unsupported_reason,
    target_path: TargetPath::GuestTagAssignedAtAttach,
    read_write: ReadWritePolicy::ReadWrite,
    sharing: sharing_of(None),
    residency: Residency::HostRam,
    // The vhost-user binding has run a live guest's workloads (Linux, AUD-29-68); the in-process seam has only
    // the simulated driver until a VMM drives it.
    conformance: if transport == GuestTransport::InheritedDescriptor && cfg!(target_os = "linux") {
      Conformance::LiveGuestWorkloads
    } else {
      Conformance::SimulatedGuestDriver
    },
    dax: dax(transport),
  }
}

/// The report for an attached device: its tag, the policy its rights allow, the semantics its guest negotiated, and DAX
/// only where its transport offers it and the device was given a window (`window`).
pub fn attached_capability(
  transport: GuestTransport,
  tag: &str,
  rights: Rights,
  (negotiated, window): (Option<InitNegotiation>, bool),
) -> TransportCapability {
  let mut report = host_capability(transport);
  report.target_path = TargetPath::GuestTag {
    tag: tag.to_owned(),
  };
  report.read_write = if rights.write {
    ReadWritePolicy::ReadWrite
  } else {
    ReadWritePolicy::ReadOnly
  };
  report.sharing = sharing_of(negotiated);
  report.dax.advertised = report.dax.advertised && window;
  report
}
