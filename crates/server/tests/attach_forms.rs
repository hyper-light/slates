//! The attachment forms and their capability report (§4.6 A-9 "Capabilities differ by host, kernel,
//! runtime and VMM and must be reported by `attach` and `status`: supported transport, target-path
//! constraints, read/write policy, sharing/cache semantics, residency boundary and conformance evidence.
//! Requesting an unsupported form returns `AttachmentUnsupported{transport, reason}`"; RQ-20; AC-4.11 /
//! T-4.13's report and refusal legs): a daemon started in this process, a client through the real
//! rendezvous, and the verbs driven over the rings. The report is checked against the machine — the
//! kernel's own `uname`, whether the daemon's loopback listener bound — never against a copy of the table.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn
)]
// These integration tests drive the daemon's NFS-loopback transport, the fleet's TCP
// transport and rustix syscalls — all macOS/Linux; on Windows the daemon mounts through WinFsp and
// the fleet transport is QUIC-over-UDP, so these particular tests are unix (as `virtiofs.rs` is).
#![cfg(unix)]

use std::time::{Duration, Instant};

use slates_ipc::protocol::{
  AttachRequest, AttachTransport, AttachmentCapability, Conformance, DeleteWhileOpen, Direction,
  Established, HostPathReason, Intent, KernelCache, NamePolicy, ReadWritePolicy, Refusal,
  ReplyBody, RequestBody, Residency, SizeClass, StatusReport, TargetPathConstraint,
  TransportReport, UnsupportedReason, VolumeId, pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_server::{Daemon, DaemonConfig, SegmentSource};
use slates_wire::request::RequestId;

mod common;

/// Shape: the reply deadline (nanoseconds): five seconds, far past any served verb.
const DEADLINE_NS: u64 = 5_000_000_000;
/// Shape: how long a client waits for the daemon or a full ring before giving up.
const CREDIT_WAIT: Duration = Duration::from_secs(5);

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
            end: ClientEnd::connected(connected),
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
  let profile = common::machine_profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{name}"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
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
  *report
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
  assert_eq!(
    nfs.sharing.delete_while_open,
    DeleteWhileOpen::SillyRenamed,
    "Appendix C: the macOS NFS client's .nfs temp files on delete-while-open"
  );
  assert_eq!(nfs.conformance, Conformance::LiveKernelMountTest);
  assert_eq!(
    entry(&report.transports, AttachTransport::Fuse).unsupported_reason,
    Some(UnsupportedReason::HostPlatform)
  );
}

/// Whether this host offers an unprivileged FUSE mount, asked the way the daemon asks (`/dev/fuse` readable
/// and writable by this process, `fusermount3` on the `PATH`; AUD-29-64).
fn fuse_on_this_host() -> bool {
  #[cfg(target_os = "linux")]
  {
    rustix::fs::access(
      "/dev/fuse",
      rustix::fs::Access::READ_OK | rustix::fs::Access::WRITE_OK,
    )
    .is_ok()
      && std::process::Command::new("sh")
        .args(["-c", "command -v fusermount3"])
        .output()
        .is_ok_and(|o| o.status.success())
  }
  #[cfg(not(target_os = "linux"))]
  {
    false
  }
}

/// Linux: an NFS mount needs a privilege slates never asks for (R10); the FUSE mount is served where the host
/// offers FUSE, with the live kernel mount as its evidence, and refused `FuseUnavailable` elsewhere (AUD-29-64).
fn assert_linux_host_mounts(report: &StatusReport) {
  assert_eq!(
    entry(&report.transports, AttachTransport::NfsLoopback).unsupported_reason,
    Some(UnsupportedReason::MountNeedsPrivilege),
    "R10: an NFS mount needs a privilege slates never asks for"
  );
  let fuse = entry(&report.transports, AttachTransport::Fuse);
  if fuse_on_this_host() {
    assert_eq!(
      fuse.unsupported_reason, None,
      "FUSE is offered on this host"
    );
    assert_eq!(fuse.conformance, Conformance::LiveKernelMountTest);
  } else {
    assert_eq!(
      fuse.unsupported_reason,
      Some(UnsupportedReason::FuseUnavailable)
    );
  }
}

/// Whether this host offers the container bind over its own FUSE mount: Linux with FUSE, where a shared mount's
/// bind ran container workloads (`a_linux_container_reaches_the_shared_mount_as_its_own_ids`).
fn linux_binds_its_fuse_mount() -> bool {
  cfg!(target_os = "linux") && fuse_on_this_host()
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

/// The refusal of a container bind request, with the volume proven untouched afterwards.
fn refused_oci(
  client: &mut Client,
  id: VolumeId,
  snapshot: Option<slates_ipc::protocol::SnapshotId>,
  source: &str,
  destination: &str,
) -> Refusal {
  let ReplyBody::Refused { refusal } = client.call(&RequestBody::Attach {
    volume: id,
    snapshot,
    intent: Intent::Write,
    form: AttachRequest::Oci {
      source: source.to_owned(),
      destination: destination.to_owned(),
    },
  }) else {
    panic!("a container bind of {source} was established");
  };
  let after = status(client, id);
  assert_eq!(after.attachments, 0, "nothing recorded");
  assert_eq!(after.lease_epoch, None, "no lease taken");
  refusal
}

/// A container bind is refused typed before any effect when the host path is not a mount point of
/// this volume (§4.4 "Attach with a chosen path that cannot be honoured: Refused
/// (`ChosenPathUnavailable{reason}`)"; §4.6 A-9 "A metadata record is insufficient evidence of a
/// usable container path"): the root directory is a foreign filesystem, a directory that is no mount
/// point is named as such, a relative path is refused before the table is read; and on a host with no
/// offered host mount the form itself is refused `AttachmentUnsupported{Oci, HostMountRequired}`.
fn assert_unbound_host_paths_are_refused_typed(client: &mut Client, id: VolumeId) {
  let root = refused_oci(client, id, None, "/", "/work");
  assert_the_root_is_no_bind_source(client, id, &root);
  let relative = refused_oci(client, id, None, "work", "/work");
  assert!(
    matches!(
      relative,
      Refusal::ChosenPathUnavailable {
        reason: HostPathReason::NotAbsolute
      } | Refusal::AttachmentUnsupported { .. }
    ),
    "a relative source never reaches the mount table: {relative:?}"
  );
}

/// The host mount presents the volume's live head, so a snapshot cannot be bound through it; where the
/// container bind itself is not offered (no host mount, or a FUSE host whose container workload is unproven)
/// that refusal comes first.
fn assert_a_snapshot_cannot_be_bound(client: &mut Client, id: VolumeId) {
  let ReplyBody::Snapshotted { id: snapshot, .. } =
    client.call(&RequestBody::Snapshot { volume: id })
  else {
    panic!("snapshot");
  };
  let refusal = refused_oci(client, id, Some(snapshot), "/", "/work");
  assert!(
    matches!(
      refusal,
      Refusal::AttachmentUnsupported {
        transport: AttachTransport::Oci,
        reason: UnsupportedReason::SnapshotNotPresentedByHostMount
          | UnsupportedReason::HostMountRequired
          | UnsupportedReason::ContainerWorkloadUnproven,
      }
    ),
    "{refusal:?}"
  );
}

/// `attach` reports the capability of the form it established: the record form under the root mount
/// is read-only for a read intent and read-write (with the lease) for a write intent; a container
/// bind whose host path is not a mount point of this volume, or whose form this host cannot offer, is
/// refused typed before any effect, so nothing is recorded — no attachment, no lease.
#[test]
fn attach_reports_its_form_and_an_unsupported_form_is_refused_typed_with_nothing_recorded() {
  let (daemon, instance) = single_shard_daemon("attach-forms-attach");
  let mut client = Client::connect(&instance);
  let id = create(&mut client, "forms");

  assert_root_attachments_report_their_policy(&mut client, id);
  assert_eq!(status(&mut client, id).attachments, 0);
  assert_unbound_host_paths_are_refused_typed(&mut client, id);
  assert_a_snapshot_cannot_be_bound(&mut client, id);

  drop(client);
  drop(daemon);
}

/// The container bind is offered exactly when a host mount is (macOS with the listener bound), with
/// the verified source export as its evidence (AUD-29-67: the runtime's profile is the handshake's); where no
/// host mount is offered it is refused
/// `HostMountRequired` — never claimed from a table.
#[test]
fn the_container_bind_is_offered_exactly_when_a_host_mount_is() {
  let (daemon, instance) = single_shard_daemon("attach-forms-oci-entry");
  let mut client = Client::connect(&instance);
  let id = create(&mut client, "forms");
  let report = status(&mut client, id);
  let oci = entry(&report.transports, AttachTransport::Oci);
  let nfs = entry(&report.transports, AttachTransport::NfsLoopback);
  if cfg!(target_os = "macos") {
    assert_eq!(oci.supported, nfs.supported);
    if oci.supported {
      assert_eq!(oci.conformance, Conformance::VerifiedSourceExport);
    }
  } else if linux_binds_its_fuse_mount() {
    assert!(oci.supported, "the shared FUSE mount's bind is offered");
    assert_eq!(oci.conformance, Conformance::VerifiedSourceExport);
  } else {
    assert_eq!(
      oci.unsupported_reason,
      Some(UnsupportedReason::HostMountRequired)
    );
  }
  drop(client);
  drop(daemon);
}

/// The refusal of a guest form requested over the ring, with the volume proven untouched afterwards.
fn refused_guest(client: &mut Client, id: VolumeId, transport: AttachTransport) -> Refusal {
  let ReplyBody::Refused { refusal } = client.call(&RequestBody::Attach {
    volume: id,
    snapshot: None,
    intent: Intent::Read,
    form: AttachRequest::Guest { transport },
  }) else {
    panic!("a guest device was established over the ring");
  };
  let after = status(client, id);
  assert_eq!(after.attachments, 0, "nothing recorded");
  refusal
}

/// The guest entries carry the device's own facts: the in-process seam served — the guest mounts a
/// tag, the bytes reach the guest's page cache, DAX is never mapped (AC-4.12), the evidence is the
/// simulated guest driver until a live guest runs (AC-9.7) — and the inherited-descriptor binding
/// refused with the device's own reason.
fn assert_guest_entries(transports: &TransportReport) {
  let in_process = entry(transports, AttachTransport::VirtioFsInProcess);
  assert!(in_process.supported, "the in-process seam is served");
  assert_eq!(in_process.target_path, TargetPathConstraint::GuestTag);
  assert_eq!(in_process.conformance, Conformance::SimulatedGuestDriver);
  assert_eq!(
    in_process.residency,
    Residency::DaemonRamAndGuestPageCache { dax_mapped: false },
    "DAX is never advertised (AC-4.12)"
  );
  assert!(in_process.sharing.server_open_state);
  assert_eq!(
    in_process.sharing.delete_while_open,
    DeleteWhileOpen::Unlinked
  );
  let inherited = entry(transports, AttachTransport::VirtioFsInheritedDescriptor);
  assert_eq!(
    inherited.unsupported_reason,
    inherited_binding_refusal(),
    "the device's own report, carried through"
  );
}

/// Why the inherited-descriptor form is unsupported here: built on Linux (vhost-user, AUD-29-68), not elsewhere.
fn inherited_binding_refusal() -> Option<UnsupportedReason> {
  (!cfg!(target_os = "linux")).then_some(UnsupportedReason::BindingNotBuilt)
}

/// A guest form asked for over the ring is refused typed: no VMM seam accompanies a ring request
/// (the harness hands it in-process), the unbuilt binding says so itself, and a transport that is
/// not a guest's is a bad request.
fn assert_guest_requests_refused(client: &mut Client, id: VolumeId) {
  assert_eq!(
    refused_guest(client, id, AttachTransport::VirtioFsInProcess),
    Refusal::AttachmentUnsupported {
      transport: AttachTransport::VirtioFsInProcess,
      reason: UnsupportedReason::SeamNotOnWire,
    }
  );
  assert_eq!(
    refused_guest(client, id, AttachTransport::VirtioFsInheritedDescriptor),
    Refusal::AttachmentUnsupported {
      transport: AttachTransport::VirtioFsInheritedDescriptor,
      reason: inherited_binding_refusal().unwrap_or(UnsupportedReason::SeamNotOnWire),
    }
  );
  assert!(
    matches!(
      refused_guest(client, id, AttachTransport::Oci),
      Refusal::BadRequest { .. }
    ),
    "a host transport is not a guest form"
  );
}

/// The guest transports report the device's own facts (§4.6 A-9 "must be reported by `attach` and
/// `status`"; the device half of GAP-A9-5, `crates/bridge-virtiofs`), and a guest form asked for
/// over the ring is refused typed with nothing recorded.
#[test]
fn the_guest_transports_report_the_devices_own_facts_and_a_ring_request_is_refused_typed() {
  let (daemon, instance) = single_shard_daemon("attach-forms-guest");
  let mut client = Client::connect(&instance);
  let id = create(&mut client, "forms");
  let report = status(&mut client, id);
  assert_guest_entries(&report.transports);
  assert_guest_requests_refused(&mut client, id);
  drop(client);
  drop(daemon);
}

/// The bytes `file` holds as the host mount at `path` presents it (the NFS export, over loopback).
#[cfg(unix)]
fn read_through_the_mount(port: u16, path: &str, file: &str) -> Vec<u8> {
  let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = common::nfs::mount(&mut stream, path, 1);
  let handle = common::nfs::lookup(&mut stream, &root, file, 2);
  common::nfs::read(&mut stream, &handle, 3)
}

/// A volume whose file `f` held `before` at its snapshot and holds `after!` now, with the connection that
/// wrote it.
struct Rewritten {
  volume: VolumeId,
  snapshot: slates_ipc::protocol::SnapshotId,
  stream: std::net::TcpStream,
  file: Vec<u8>,
}

/// Creates volume `name`, writes `before` into `f` through its own mount, snapshots it, then writes `after!`.
#[cfg(unix)]
fn rewritten_after_a_snapshot(
  daemon: &Daemon,
  client: &mut Client,
  port: u16,
  name: &str,
) -> Rewritten {
  let volume = create(client, name);
  let capability = daemon.mount_capability(name).unwrap().unwrap();
  let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = common::nfs::mount(&mut stream, &capability, 1);
  let file = common::nfs::create(&mut stream, &root, "f", 2);
  common::nfs::write(&mut stream, &file, b"before", 3);
  let ReplyBody::Snapshotted { id: snapshot, .. } = client.call(&RequestBody::Snapshot { volume })
  else {
    panic!("snapshot");
  };
  common::nfs::write(&mut stream, &file, b"after!", 4);
  Rewritten {
    volume,
    snapshot,
    stream,
    file,
  }
}

/// Attaches `snapshot` of `volume` (named `name`) as a read host mount: the attachment and its capability path.
#[cfg(unix)]
fn snapshot_mount(
  client: &mut Client,
  volume: VolumeId,
  snapshot: slates_ipc::protocol::SnapshotId,
  name: &str,
) -> (u64, String) {
  let ReplyBody::Attached {
    attachment,
    token: Some(token),
    ..
  } = client.call(&RequestBody::Attach {
    volume,
    snapshot: Some(snapshot),
    intent: Intent::Read,
    form: AttachRequest::HostMount,
  })
  else {
    panic!("the snapshot's read mount attaches");
  };
  let token_hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
  (attachment, format!("/{name}@{attachment:x}.{token_hex}"))
}

/// A write intent on a snapshot is refused typed: a snapshot is immutable.
#[cfg(unix)]
fn assert_a_snapshot_is_never_attached_for_a_write(
  client: &mut Client,
  volume: VolumeId,
  snapshot: slates_ipc::protocol::SnapshotId,
) {
  let write_intent = client.call(&RequestBody::Attach {
    volume,
    snapshot: Some(snapshot),
    intent: Intent::Write,
    form: AttachRequest::HostMount,
  });
  assert!(
    matches!(
      write_intent,
      ReplyBody::Refused {
        refusal: Refusal::AttachmentUnsupported {
          reason: UnsupportedReason::SnapshotNotPresentedByHostMount,
          ..
        },
        ..
      }
    ),
    "{write_intent:?}"
  );
}

/// The snapshot mount at `path` reads `before`, refuses a write, and still reads `before` after it.
#[cfg(unix)]
fn assert_the_mount_presents_the_snapshot_read_only(port: u16, path: &str) {
  assert_eq!(read_through_the_mount(port, path, "f"), b"before");
  let mut view = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
  let view_root = common::nfs::mount(&mut view, path, 1);
  let view_file = common::nfs::lookup(&mut view, &view_root, "f", 2);
  assert_ne!(
    common::nfs::write_status(&mut view, &view_file, b"wrong!", 3),
    0,
    "a write through the snapshot's mount is refused"
  );
  assert_eq!(read_through_the_mount(port, path, "f"), b"before");
}

/// AUD-29-76. Do: write `before` into a file through the volume's own mount, snapshot, write `after!`; attach the
/// snapshot as a host mount for a read and for a write; read and write the file through the read mount; destroy
/// the snapshot while it is mounted and again after the detach. Expect: the read mount presents the snapshot —
/// `before`, never the head's `after!` (before 2026-10-01 it was refused; earlier still, attached and showing
/// the head); a write through it is refused and leaves the snapshot unchanged; a write intent is refused typed
/// (a snapshot is immutable); the snapshot cannot be destroyed while a mount presents it (`Pinned`), and can
/// once the mount is detached; the head still reads `after!`.
#[cfg(unix)]
#[test]
fn a_snapshot_host_mount_presents_the_snapshot_read_only_and_pins_it() {
  let (daemon, instance) = single_shard_daemon("attach-forms-snapshot-mount");
  let Some(port) = daemon.nfs_port() else {
    eprintln!("SKIP: the loopback export did not bind, so no host mount is offered here");
    return;
  };
  let mut client = Client::connect(&instance);
  let Rewritten {
    volume: id,
    snapshot,
    mut stream,
    file,
  } = rewritten_after_a_snapshot(&daemon, &mut client, port, "pinned");
  assert_a_snapshot_is_never_attached_for_a_write(&mut client, id, snapshot);
  let (attachment, path) = snapshot_mount(&mut client, id, snapshot, "pinned");
  assert_the_mount_presents_the_snapshot_read_only(port, &path);
  let destroy = RequestBody::DestroySnapshot {
    volume: id,
    snapshot,
  };
  assert!(
    matches!(client.call(&destroy), ReplyBody::Refused { .. }),
    "a mounted snapshot is pinned"
  );
  assert!(matches!(
    client.call(&RequestBody::Detach { attachment }),
    ReplyBody::Detached
  ));
  assert!(
    matches!(client.call(&destroy), ReplyBody::SnapshotDestroyed),
    "the detach unpinned it"
  );
  assert_eq!(
    common::nfs::read(&mut stream, &file, 5),
    b"after!",
    "the head is untouched"
  );
  drop(client);
  drop(daemon);
}

/// AUD-29-76 (a snapshot mount across a restart). Do: as above, write `before`, snapshot, write `after!`, attach
/// the snapshot as a read host mount; stop the daemon (a stop publishes nothing) and start a second over the same
/// anchor segment; read the file through the same mount capability. Expect: the second daemon rebuilt the
/// mount's view from its record, so the capability still presents `before`, not the head.
#[cfg(unix)]
#[test]
fn a_snapshot_host_mount_presents_the_snapshot_again_after_a_restart() {
  use common::anchor::{anchor_segment, source_of};
  let profile = common::machine_profile();
  let instance = format!("srv-snapshot-view-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let segment = anchor_segment("snapshot-view", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let Some(port) = first.nfs_port() else {
    eprintln!("SKIP: the loopback export did not bind, so no host mount is offered here");
    return;
  };
  let mut client = Client::connect(&instance);
  let rewritten = rewritten_after_a_snapshot(&first, &mut client, port, "kept");
  let (_, path) = snapshot_mount(&mut client, rewritten.volume, rewritten.snapshot, "kept");
  let stream = rewritten.stream;
  assert_eq!(read_through_the_mount(port, &path, "f"), b"before");
  drop(stream);
  drop(client);
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let port = second.nfs_port().expect("the second daemon serves NFS");
  assert_eq!(
    read_through_the_mount(port, &path, "f"),
    b"before",
    "the view was rebuilt from the record"
  );
  second.stop();
  drop(segment);
}

/// Attaches the subtree `subtree` of `volume` (named `name`) as a host mount: its capability path.
#[cfg(unix)]
fn scoped_mount(client: &mut Client, volume: VolumeId, subtree: &str, name: &str) -> String {
  let ReplyBody::Attached {
    attachment,
    token: Some(token),
    ..
  } = client.call(&RequestBody::Attach {
    volume,
    snapshot: None,
    intent: Intent::Write,
    form: AttachRequest::ScopedHostMount {
      subtree: subtree.to_owned(),
    },
  })
  else {
    panic!("the subtree mount attaches");
  };
  let token_hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
  format!("/{name}@{attachment:x}.{token_hex}")
}

/// The handle `fh` would be had the attachment that minted `capability_fh` minted it: the forgery a client of a
/// scoped mount can make, since an object's number is no secret.
#[cfg(unix)]
fn forged(fh: &[u8], capability_fh: &[u8]) -> Vec<u8> {
  use slates_bridge_nfs::handle::FileHandle;
  use slates_bridge_nfs::nfs::Nfsfh3;
  let target = FileHandle::from_fh(&Nfsfh3(fh.to_vec())).unwrap();
  let capability = FileHandle::from_fh(&Nfsfh3(capability_fh.to_vec())).unwrap();
  FileHandle {
    attachment: capability.attachment,
    token: capability.token,
    ..target
  }
  .to_fh()
  .0
}

/// AUD-29-76 (a subtree mount). Do: through the volume's own mount make `shared/g` (holding `inside`) and
/// `private/secret`; attach `/shared` as a host mount; mount it, list and read its root, write a file, look up
/// `..` at its root; forge handles to `private` and to the volume's root with the scoped capability and ask
/// GETATTR, LOOKUP and READDIRPLUS through them; attach a file and a missing path as subtrees. Expect: the
/// mount's root is `shared` (it lists `g`, reads `inside`, and the write lands in `shared` as the owner sees
/// it); `..` at its root reaches nothing above it; every forged handle is refused (before 2026-10-01 a mount
/// could present only the volume's root, so no subtree could be handed out at all); a file is refused
/// `NotDirectory` and a missing path `NotFound`, with nothing attached.
#[cfg(unix)]
#[test]
fn a_scoped_host_mount_reaches_nothing_outside_its_directory() {
  let (daemon, instance) = single_shard_daemon("attach-forms-scoped-mount");
  let Some(port) = daemon.nfs_port() else {
    eprintln!("SKIP: the loopback export did not bind, so no host mount is offered here");
    return;
  };
  let mut client = Client::connect(&instance);
  let volume = create(&mut client, "scoped");
  let owner_path = daemon.mount_capability("scoped").unwrap().unwrap();
  let mut owner = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
  let owner_root = common::nfs::mount(&mut owner, &owner_path, 1);
  let shared = common::nfs::mkdir(&mut owner, &owner_root, "shared", 2);
  let private = common::nfs::mkdir(&mut owner, &owner_root, "private", 3);
  let secret = common::nfs::create(&mut owner, &private, "secret", 4);
  common::nfs::write(&mut owner, &secret, b"hidden", 5);
  let inside = common::nfs::create(&mut owner, &shared, "g", 6);
  common::nfs::write(&mut owner, &inside, b"inside", 7);

  let path = scoped_mount(&mut client, volume, "/shared", "scoped");
  let mut scoped = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = common::nfs::mount(&mut scoped, &path, 1);
  assert_eq!(
    common::nfs::readdirplus(&mut scoped, &root, 2),
    [".", "..", "g"]
  );
  assert_eq!(read_through_the_mount(port, &path, "g"), b"inside");
  let written = common::nfs::create(&mut scoped, &root, "h", 3);
  common::nfs::write(&mut scoped, &written, b"landed", 4);
  let seen = common::nfs::lookup(&mut owner, &shared, "h", 8);
  assert_eq!(common::nfs::read(&mut owner, &seen, 9), b"landed");
  if let Ok(above) = common::nfs::lookup_status(&mut scoped, &root, "..", 5) {
    assert_eq!(
      common::nfs::readdirplus(&mut scoped, &above, 6),
      [".", "..", "g", "h"],
      "`..` at the mount's root is the root itself"
    );
  }

  assert_forged_handles_are_refused(&mut scoped, &root, [&private, &secret, &owner_root]);
  assert_a_subtree_must_be_a_directory(&mut client, volume);
  drop(client);
  drop(daemon);
}

/// Every handle in `outside` (private, its secret, the volume's root), forged with the scoped mount's capability
/// (`root`), is refused by GETATTR, LOOKUP and READ alike.
#[cfg(unix)]
fn assert_forged_handles_are_refused(
  scoped: &mut std::net::TcpStream,
  root: &[u8],
  outside: [&Vec<u8>; 3],
) {
  let [private, secret, _] = outside;
  for handle in outside {
    let forgery = forged(handle, root);
    assert!(
      common::nfs::owner_and_mode_status(scoped, &forgery, 7).is_err(),
      "GETATTR of a handle outside the subtree is refused"
    );
  }
  assert!(common::nfs::lookup_status(scoped, &forged(private, root), "secret", 8).is_err());
  assert_ne!(
    common::nfs::read_status(scoped, &forged(secret, root), 9),
    0,
    "READ of a file outside the subtree is refused"
  );
}

/// A subtree naming a file is refused `NotDirectory` and one naming nothing `NotFound`, with nothing attached.
#[cfg(unix)]
fn assert_a_subtree_must_be_a_directory(client: &mut Client, volume: VolumeId) {
  for (subtree, expected) in [("/shared/g", "NotDirectory"), ("/absent", "NotFound")] {
    let refused = client.call(&RequestBody::Attach {
      volume,
      snapshot: None,
      intent: Intent::Read,
      form: AttachRequest::ScopedHostMount {
        subtree: subtree.to_owned(),
      },
    });
    let ReplyBody::Refused { refusal, .. } = refused else {
      panic!("{subtree} attached: {refused:?}");
    };
    assert!(
      format!("{refusal:?}").contains(expected),
      "{subtree}: {refusal:?}"
    );
  }
}

/// Takes a snapshot of `volume`: its id.
#[cfg(unix)]
fn snapshot_of(client: &mut Client, volume: VolumeId) -> slates_ipc::protocol::SnapshotId {
  let ReplyBody::Snapshotted { id, .. } = client.call(&RequestBody::Snapshot { volume }) else {
    panic!("snapshot");
  };
  id
}

/// `advance(attachment, version)`: the version now presented and the paths invalidated, or the refusal.
#[cfg(unix)]
fn advance(
  client: &mut Client,
  attachment: u64,
  version: u64,
) -> Result<(u64, Vec<String>), Refusal> {
  match client.call(&RequestBody::Advance {
    attachment,
    version: Some(version),
  }) {
    ReplyBody::Advanced {
      version,
      invalidated,
    } => Ok((version, invalidated)),
    ReplyBody::Refused { refusal, .. } => Err(refusal),
    other => panic!("advance answered {other:?}"),
  }
}

/// AUD-29-76 (`advance` of a snapshot mount). Do: write `before` into `f`, snapshot (`first`), rewrite
/// `f` `middle`, snapshot (`second`), rewrite `f` `after!`; mount `first` read-only; advance the attachment to a
/// snapshot that does not exist, then to `second`; read through the same capability; destroy each snapshot;
/// restart the daemon over the same anchor segment and read again. Expect: the missing snapshot is refused and
/// the mount still reads `before`; the advance answers `second` and names `/f` alone (neither the root nor anything else changed);
/// the same capability then reads `middle`, never the head's `after!`; `first` is unpinned (destroyable) and
/// `second` pinned (`Pinned`); after the restart the mount still presents `second`. Before 2026-10-01 a snapshot
/// mount could not move (`advance` answered `NotGreen`).
#[cfg(unix)]
#[test]
fn a_snapshot_mount_advances_to_a_later_snapshot_and_names_what_changed() {
  use common::anchor::{anchor_segment, source_of};
  let profile = common::machine_profile();
  let instance = format!("srv-snapshot-advance-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let segment = anchor_segment("snapshot-advance", &profile, &config);
  let first_daemon = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first_daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let Some(port) = first_daemon.nfs_port() else {
    eprintln!("SKIP: the loopback export did not bind, so no host mount is offered here");
    return;
  };
  let mut client = Client::connect(&instance);
  let Rewritten {
    volume,
    snapshot: first,
    mut stream,
    file,
  } = rewritten_after_a_snapshot(&first_daemon, &mut client, port, "advancing");
  common::nfs::write(&mut stream, &file, b"middle", 10);
  let second = snapshot_of(&mut client, volume);
  common::nfs::write(&mut stream, &file, b"after!", 11);
  let (attachment, path) = snapshot_mount(&mut client, volume, first, "advancing");
  advance_and_check_the_pins(
    &mut client,
    port,
    (attachment, &path),
    volume,
    (first, second),
  );
  drop(stream);
  drop(client);
  first_daemon.stop();

  let second_daemon = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let port = second_daemon
    .nfs_port()
    .expect("the second daemon serves NFS");
  assert_eq!(
    read_through_the_mount(port, &path, "f"),
    b"middle",
    "the re-pin was recorded and its view rebuilt"
  );
  second_daemon.stop();
  drop(segment);
}

/// The mount of `first` at `path` reads `before`; an advance to a missing snapshot is refused and changes
/// nothing; the advance to `second` names `/f` alone and the mount then reads `middle`; `first` is unpinned
/// and `second` pinned.
#[cfg(unix)]
fn advance_and_check_the_pins(
  client: &mut Client,
  port: u16,
  (attachment, path): (u64, &str),
  volume: VolumeId,
  (first, second): (
    slates_ipc::protocol::SnapshotId,
    slates_ipc::protocol::SnapshotId,
  ),
) {
  assert_eq!(read_through_the_mount(port, path, "f"), b"before");
  assert!(advance(client, attachment, second.value ^ 1).is_err());
  assert_eq!(read_through_the_mount(port, path, "f"), b"before");
  assert_eq!(
    advance(client, attachment, second.value),
    Ok((second.value, vec!["/f".to_owned()]))
  );
  assert_eq!(read_through_the_mount(port, path, "f"), b"middle");
  let destroy =
    |client: &mut Client, snapshot| client.call(&RequestBody::DestroySnapshot { volume, snapshot });
  assert!(matches!(
    destroy(client, first),
    ReplyBody::SnapshotDestroyed
  ));
  assert!(matches!(destroy(client, second), ReplyBody::Refused { .. }));
}

/// The root directory as a bind source, by this host's rule: a foreign filesystem where a host mount is offered
/// (APFS on macOS; whatever the root is on a Linux host whose FUSE mount binds), else the form itself refused.
fn assert_the_root_is_no_bind_source(client: &mut Client, id: VolumeId, root: &Refusal) {
  if cfg!(target_os = "macos") {
    assert!(
      matches!(
        root,
        Refusal::ChosenPathUnavailable {
          reason: HostPathReason::ForeignFilesystem { fstype }
        } if fstype == "apfs"
      ),
      "the root is APFS, not a slates mount: {root:?}"
    );
    assert_eq!(
      refused_oci(client, id, None, "/private", "/work"),
      Refusal::ChosenPathUnavailable {
        reason: HostPathReason::NotAMountPoint
      },
      "a directory that is not a mount point"
    );
  } else if linux_binds_its_fuse_mount() {
    assert!(
      matches!(
        root,
        Refusal::ChosenPathUnavailable {
          reason: HostPathReason::ForeignFilesystem { .. }
        }
      ),
      "the root is no slates mount: {root:?}"
    );
  } else {
    assert_eq!(
      root,
      &Refusal::AttachmentUnsupported {
        transport: AttachTransport::Oci,
        reason: UnsupportedReason::HostMountRequired,
      },
      "no host mount is offered on this platform"
    );
  }
}
