//! The attachment forms and their capability report (§4.6 A-9 "Capabilities differ by host, kernel,
//! runtime and VMM and must be reported by `attach` and `status`: supported transport, target-path
//! constraints, read/write policy, sharing/cache semantics, residency boundary and conformance evidence.
//! Requesting an unsupported form returns `AttachmentUnsupported{transport, reason}`"; RQ-20; AC-4.11 /
//! T-4.13's report and refusal legs): a daemon started in this process, a client through the real
//! rendezvous, and the verbs driven over the rings. The report is checked against the machine — the
//! kernel's own `uname`, whether the daemon's loopback listener bound — never against a copy of the table.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use slates_ipc::protocol::{
  AttachRequest, AttachTransport, AttachmentCapability, Conformance, Direction, Established,
  Intent, KernelCache, NamePolicy, ReadWritePolicy, Refusal, ReplyBody, RequestBody, SizeClass,
  StatusReport, TargetPathConstraint, TransportReport, UnsupportedReason, VolumeId, pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};
use slates_wire::request::RequestId;

/// Shape: the probe budget of the quick profile (milliseconds); an input to derivations, not a gate.
const PROBE_MS: u64 = 5;
/// Shape: the reply deadline (nanoseconds): five seconds, far past any served verb.
const DEADLINE_NS: u64 = 5_000_000_000;
/// Shape: how long a client waits for the daemon or a full ring before giving up.
const CREDIT_WAIT: Duration = Duration::from_secs(5);

fn profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
}

// --- The client side of the daemon's own rendezvous (as in tests/daemon.rs). ---

struct Client {
  end: ClientEnd,
  client: u32,
  sequence: u32,
}

impl Client {
  fn connect(instance: &str) -> Client {
    let started = Instant::now();
    loop {
      match connect(instance) {
        Ok(connected) => {
          let client = connected.region.client_id();
          return Client {
            end: ClientEnd::with_doorbell(connected.region, connected.doorbell),
            client,
            sequence: 0,
          };
        }
        Err(IpcError::DaemonUnavailable { .. }) if started.elapsed() < CREDIT_WAIT => {
          std::hint::spin_loop();
        }
        Err(e) => panic!("{e}"),
      }
    }
  }

  fn call(&mut self, body: &RequestBody) -> ReplyBody {
    self.sequence += 1;
    let id = RequestId {
      client: self.client,
      sequence: self.sequence,
    };
    let index = self.end.next_request_index();
    let slot = pack(
      self.end.region_mut(),
      Direction::Request,
      index,
      id.word(),
      body,
    )
    .unwrap();
    let started = Instant::now();
    loop {
      match self.end.send(&slot) {
        Ok(()) => break,
        Err(IpcError::RingFull) if started.elapsed() < CREDIT_WAIT => std::hint::spin_loop(),
        Err(e) => panic!("{e}"),
      }
    }
    let reply = self.end.wait(Some(DEADLINE_NS)).unwrap();
    unpack(self.end.region(), reply.kind, &reply.payload).unwrap()
  }
}

fn single_shard_daemon(name: &str) -> (Daemon, String) {
  let profile = profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance).with_shards(1);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{name}"),
    },
  )
  .unwrap();
  (daemon, instance)
}

fn scratch(name: &str) -> RequestBody {
  RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

fn create(client: &mut Client, name: &str) -> VolumeId {
  let ReplyBody::Created { id } = client.call(&scratch(name)) else {
    panic!("the volume was not created");
  };
  id
}

fn status(client: &mut Client, volume: VolumeId) -> StatusReport {
  let ReplyBody::Status { report } = client.call(&RequestBody::Status { volume }) else {
    panic!("status");
  };
  report
}

/// The report's entry for one transport; every transport is reported, supported or not.
fn entry(report: &TransportReport, transport: AttachTransport) -> &AttachmentCapability {
  report
    .capabilities
    .iter()
    .find(|c| c.transport == transport)
    .unwrap_or_else(|| panic!("{transport:?} is reported: {report:?}"))
}

/// The host facts are the kernel's own words, never a copy of a table.
fn assert_host_facts(transports: &TransportReport) {
  let uts = rustix::system::uname();
  assert_eq!(transports.os, uts.sysname().to_string_lossy());
  assert_eq!(
    transports.kernel.as_deref(),
    Some(uts.release().to_string_lossy().as_ref()),
    "the kernel release as uname states it"
  );
}

/// Every entry is supported exactly when it carries no reason, and every transport shares the one
/// owning shard's view (D-7); the record form is offered to the owner read-write under the root mount.
fn assert_every_entry_consistent(transports: &TransportReport) {
  for capability in &transports.capabilities {
    assert_eq!(
      capability.supported,
      capability.unsupported_reason.is_none(),
      "supported exactly when no reason: {capability:?}"
    );
    assert!(capability.sharing.one_owning_shard, "D-7: {capability:?}");
  }
  let root = entry(transports, AttachTransport::Root);
  assert!(root.supported);
  assert_eq!(root.target_path, TargetPathConstraint::RootMount);
  assert_eq!(
    root.read_write,
    ReadWritePolicy::ReadWrite,
    "the owner may write"
  );
  assert_eq!(root.conformance, Conformance::VerbLifecycleTest);
}

/// macOS: the NFS loopback mount exists exactly when the daemon's listener bound — at a user-owned
/// directory, cached by the client's timeouts, with the live kernel mount as its evidence; FUSE is
/// another platform's.
fn assert_macos_host_mounts(report: &StatusReport) {
  let nfs = entry(&report.transports, AttachTransport::NfsLoopback);
  assert_eq!(
    nfs.supported,
    report.nfs_port.is_some(),
    "the mount exists exactly when the listener bound"
  );
  assert_eq!(
    nfs.target_path,
    TargetPathConstraint::UserOwnedExistingDirectory
  );
  assert_eq!(nfs.sharing.cache, KernelCache::ClientTimeouts);
  assert_eq!(nfs.conformance, Conformance::LiveKernelMountTest);
  assert_eq!(
    entry(&report.transports, AttachTransport::Fuse).unsupported_reason,
    Some(UnsupportedReason::HostPlatform)
  );
}

/// Linux: an NFS mount needs a privilege slates never asks for (R10); the FUSE bridge is not served
/// by the daemon yet.
fn assert_linux_host_mounts(report: &StatusReport) {
  assert_eq!(
    entry(&report.transports, AttachTransport::NfsLoopback).unsupported_reason,
    Some(UnsupportedReason::MountNeedsPrivilege),
    "R10: an NFS mount needs a privilege slates never asks for"
  );
  assert_eq!(
    entry(&report.transports, AttachTransport::Fuse).unsupported_reason,
    Some(UnsupportedReason::BridgeNotWired)
  );
}

/// On every Unix host: WinFsp does not exist, and the container bind's view is the host mount's at a
/// destination inside the container.
fn assert_unix_common_entries(transports: &TransportReport) {
  assert_eq!(
    entry(transports, AttachTransport::WinFsp).unsupported_reason,
    Some(UnsupportedReason::HostPlatform)
  );
  let oci = entry(transports, AttachTransport::Oci);
  assert_eq!(oci.target_path, TargetPathConstraint::ContainerDestination);
  assert_eq!(oci.sharing.cache, KernelCache::InheritedFromHostMount);
}

/// `status` reports the host facts as the kernel states them and every transport with the six facts
/// of §4.6 A-9: a transport is supported exactly when it carries no refusal reason; the NFS loopback
/// mount is supported on macOS exactly when the daemon's listener bound and is refused on Linux for the
/// privilege slates never asks for (R10); FUSE is a Linux transport the daemon does not serve yet;
/// WinFsp never exists on a Unix host; the container bind's view is the host mount's.
#[test]
fn status_reports_the_host_facts_and_every_transport_with_its_six_facts() {
  let (daemon, instance) = single_shard_daemon("attach-forms-status");
  let mut client = Client::connect(&instance);
  let id = create(&mut client, "forms");
  let report = status(&mut client, id);

  assert_host_facts(&report.transports);
  assert_every_entry_consistent(&report.transports);
  if cfg!(target_os = "macos") {
    assert_macos_host_mounts(&report);
  }
  if cfg!(target_os = "linux") {
    assert_linux_host_mounts(&report);
  }
  assert_unix_common_entries(&report.transports);

  drop(client);
  drop(daemon);
}

/// The reply of an attach in the record form: (attachment, lease epoch, what was established, the
/// capability).
fn attach_root(
  client: &mut Client,
  volume: VolumeId,
  intent: Intent,
) -> (u64, Option<u64>, Established, AttachmentCapability) {
  let ReplyBody::Attached {
    attachment,
    lease_epoch,
    established,
    capability,
    ..
  } = client.call(&RequestBody::Attach {
    volume,
    snapshot: None,
    intent,
    form: AttachRequest::Root,
  })
  else {
    panic!("attach in the record form");
  };
  (attachment, lease_epoch, established, capability)
}

fn detach(client: &mut Client, attachment: u64) {
  assert!(matches!(
    client.call(&RequestBody::Detach { attachment }),
    ReplyBody::Detached
  ));
}

/// A read attachment takes no lease and is reported read-only in the record form; a write attachment
/// takes the lease and is reported read-write.
fn assert_root_attachments_report_their_policy(client: &mut Client, id: VolumeId) {
  let (reader, lease_epoch, established, capability) = attach_root(client, id, Intent::Read);
  assert_eq!(lease_epoch, None);
  assert_eq!(established, Established::Record);
  assert_eq!(capability.transport, AttachTransport::Root);
  assert!(capability.supported);
  assert_eq!(capability.read_write, ReadWritePolicy::ReadOnly);
  detach(client, reader);

  let (writer, lease_epoch, _, capability) = attach_root(client, id, Intent::Write);
  assert_eq!(lease_epoch, Some(1));
  assert_eq!(capability.read_write, ReadWritePolicy::ReadWrite);
  detach(client, writer);
}

/// A container bind is not established by this daemon yet: refused typed, before any effect.
fn assert_unbuilt_form_refused_with_nothing_recorded(client: &mut Client, id: VolumeId) {
  let ReplyBody::Refused { refusal } = client.call(&RequestBody::Attach {
    volume: id,
    snapshot: None,
    intent: Intent::Write,
    form: AttachRequest::Oci {
      source: "/".to_owned(),
      destination: "/work".to_owned(),
    },
  }) else {
    panic!("a container bind was established by a daemon that cannot");
  };
  assert_eq!(
    refusal,
    Refusal::AttachmentUnsupported {
      transport: AttachTransport::Oci,
      reason: UnsupportedReason::BridgeNotWired,
    }
  );
  let after = status(client, id);
  assert_eq!(after.attachments, 0, "nothing recorded");
  assert_eq!(after.lease_epoch, None, "no lease taken");
}

/// `attach` reports the capability of the form it established: the record form under the root mount
/// is read-only for a read intent and read-write (with the lease) for a write intent; a form the daemon
/// does not establish is refused `AttachmentUnsupported{transport, reason}` before any effect, so
/// nothing is recorded — no attachment, no lease.
#[test]
fn attach_reports_its_form_and_an_unsupported_form_is_refused_typed_with_nothing_recorded() {
  let (daemon, instance) = single_shard_daemon("attach-forms-attach");
  let mut client = Client::connect(&instance);
  let id = create(&mut client, "forms");

  assert_root_attachments_report_their_policy(&mut client, id);
  assert_eq!(status(&mut client, id).attachments, 0);
  assert_unbuilt_form_refused_with_nothing_recorded(&mut client, id);

  drop(client);
  drop(daemon);
}
