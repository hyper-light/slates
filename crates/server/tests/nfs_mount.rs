//! The daemon serves a provisioned volume over NFS (§4.6, R5, AC-3.10/3.12 in spirit — with no kernel
//! mount, so it runs in CI on any host): a single-shard daemon is started, a client provisions a volume
//! through the real rendezvous, and then — over the daemon's own NFS loopback port — a client mounts the
//! volume, creates a file in it, writes bytes, and reads them back. The bytes travel client → NFS →
//! `ShardVolumeSet` → the shard's real volume and back, so this proves the daemon's NFS transport
//! reaches the volumes it provisioned, not a demo volume. A real `mount_nfs localhost:PORT` would do the
//! same over the kernel; the hand-rolled ONC RPC client here needs no privilege.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// These integration tests drive the daemon's NFS-loopback transport, the fleet's TCP
// transport and rustix syscalls — all macOS/Linux; on Windows the daemon mounts through WinFsp and
// the fleet transport is QUIC-over-UDP, so these particular tests are unix (as `virtiofs.rs` is).
#![cfg(unix)]

use std::net::TcpStream;
use std::time::{Duration, Instant};

use slates_ipc::protocol::{
  AttachRequest, Direction, Intent, NamePolicy, ReplyBody, RequestBody, SizeClass,
  SnapshotBoundary, VolumeId, pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};
use slates_wire::request::RequestId;

mod common;
use common::nfs::{
  MOUNT_PROGRAM, call, create, lookup, mount, opaque, read, read_status, readdirplus, status, umnt,
  write,
};

/// Shape: the probe budget of the quick profile (milliseconds); an input to derivations, not a gate.
const PROBE_MS: u64 = 5;
/// Shape: the reply deadline (nanoseconds): five seconds, far past any served verb.
const DEADLINE_NS: u64 = 5_000_000_000;
/// Shape: how long a client waits for the daemon or a full ring before giving up.
const CREDIT_WAIT: Duration = Duration::from_secs(5);
/// Format: `MNT3ERR_NOENT` (RFC 1813 §5.1.5) — what a `MNT` of a path the caller's capability does not
/// make visible answers (AUD-01: a name without its capability is no entry, so nothing is disclosed).
const MNT3ERR_NOENT: u32 = 2;
/// Format: `NFS3ERR_ACCES` (RFC 1813 §2.6) — what a request through a handle whose capability does not
/// authorize the volume answers (AUD-01).
const NFS3ERR_ACCES: u32 = 13;

fn profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures")
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
  let profile = profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  // One shard, so every provisioned volume lands on the shard the NFS listener is served on (R8, the
  // laptop-degenerate case the daemon-side NFS serve covers; cross-shard is owed).
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

fn two_shard_daemon(name: &str) -> (Daemon, String) {
  let profile = profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  // Two shards, so a volume can land on a shard other than the one the NFS listener is served on,
  // exercising the cross-shard bridge queue.
  let config = DaemonConfig::derive(&profile, &instance, Some(2));
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

/// The daemon mounts a volume it provisioned, and a file written over NFS reads back over NFS.
#[test]
fn the_daemon_serves_a_provisioned_volume_over_nfs() {
  let (daemon, instance) = single_shard_daemon("nfsmount");
  let mut client = Client::connect(&instance);

  let ReplyBody::Created { .. } = client.call(&scratch("vol")) else {
    panic!("the volume was not created");
  };
  let port = daemon.nfs_port().expect("the daemon is serving NFS");

  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  // Mount the volume by its provisioned friendly name under its mount capability (AUD-01), not its id.
  let root_fh = mount(&mut stream, &capability_path(&daemon, "vol"), 1);
  let file_fh = create(&mut stream, &root_fh, "hello.txt", 2);
  let payload = b"written through the NFS mount into a daemon-provisioned volume\n";
  write(&mut stream, &file_fh, payload, 3);
  let got = read(&mut stream, &file_fh, 4);
  assert_eq!(
    got, payload,
    "read back over NFS the bytes written over NFS to the daemon's own volume"
  );

  drop(stream);
  drop(client);
  drop(daemon);
}

/// AC (§4.2 "admission stops under pressure"; admission.md §5.5; GAP-A9-1): a memory-pressure hold on
/// the shard's byte budget refuses a **new** admission while an **admitted** volume's within-entitlement
/// writes still land. A bounded volume is created and a file written through its mount (its reservation
/// backs it). A hold that zeroes the shard's admittable is set (as the sampler would under real
/// pressure); a new bounded create is refused `BudgetExceeded`, but the mounted volume's further write
/// within its limit still lands — its reservation is committed and the hold touches no committed claim.
/// The hold released, a new create succeeds again. Non-vacuous: the create is refused only while the
/// hold stands, and the admitted volume's write never fails.
#[test]
fn a_memory_pressure_hold_refuses_new_admission_but_not_an_admitted_volumes_writes() {
  let (daemon, instance) = single_shard_daemon("pressure");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { .. } = client.call(&scratch("kept")) else {
    panic!("the volume was not created");
  };
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = mount(&mut stream, &capability_path(&daemon, "kept"), 1);
  let file = create(&mut stream, &root, "f", 2);
  write(&mut stream, &file, b"before the hold\n", 3);

  // Under memory pressure the control shard holds the whole admittable (the sampler's effect,
  // driven directly here). A new bounded create is refused; the admitted volume is untouched.
  daemon
    .inject_pressure_hold(u64::MAX)
    .expect("the hold is installed on every shard");
  assert_eq!(
    daemon.pressure_holds().expect("the shards answer"),
    vec![u64::MAX],
    "the hold stands on the shard"
  );
  let refused = client.call(&scratch("under-pressure"));
  assert!(
    matches!(
      refused,
      ReplyBody::Refused {
        refusal: slates_ipc::protocol::Refusal::BudgetExceeded { .. }
      }
    ),
    "a new admission is refused under the pressure hold: {refused:?}"
  );
  // The admitted volume's within-entitlement write still lands — its reservation is committed.
  write(
    &mut stream,
    &file,
    b"during the hold: still within entitlement\n",
    4,
  );
  assert_eq!(
    read(&mut stream, &file, 5),
    b"during the hold: still within entitlement\n",
    "the admitted volume serves through the hold"
  );

  // Released, admission resumes.
  daemon
    .inject_pressure_hold(0)
    .expect("the hold is released on every shard");
  let ReplyBody::Created { .. } = client.call(&scratch("after-pressure")) else {
    panic!("a new volume is admitted once the hold is released");
  };
  daemon.stop();
}

/// MKDIR's directory, name and mode-only attributes; ownership comes from AUTH_SYS.
fn mkdir_args(directory: &[u8], name: &str) -> Vec<u8> {
  let mut args = Vec::new();
  opaque(directory, &mut args);
  opaque(name.as_bytes(), &mut args);
  for word in [1_u32, 0o700, 0, 0, 0, 0, 0] {
    args.extend_from_slice(&word.to_be_bytes());
  }
  args
}

/// A Unix caller owns its new directory, can populate it, and excludes other callers.
fn assert_creation_as(stream: &mut TcpStream, root: &[u8], uid: u32) {
  use common::nfs::{NFS_PROGRAM, owner_and_mode, read_opaque};
  let args = mkdir_args(root, &format!("user-{uid}"));
  let reply = call_as(stream, NFS_PROGRAM, 9, &args, 3, uid);
  assert_eq!(status(&reply), 0, "first MKDIR by uid {uid}");
  let directory = read_opaque(&reply, 8).0;
  let ownership = owner_and_mode(stream, &directory, 4);
  assert_eq!(ownership, (0o700, uid, uid));
  let nested = mkdir_args(&directory, "child");
  assert_eq!(status(&call_as(stream, NFS_PROGRAM, 9, &nested, 5, uid)), 0);
  assert_eq!(
    status(&call_as(
      stream,
      NFS_PROGRAM,
      9,
      &mkdir_args(&directory, "foreign"),
      6,
      uid + 1
    )),
    NFS3ERR_ACCES,
    "a different caller cannot create in the private directory"
  );
}

/// AC-3.10 / §4.6: each creation takes the requesting Unix user's ownership, even when MOUNT
/// admitted the shared attachment under another identity. The same mount serves multiple users;
/// nested creation must work and another user must still be refused by its directory's mode.
#[test]
fn a_mount_preserves_each_requests_unix_owner_across_shards() {
  use common::nfs::NFS_PROGRAM;
  let (daemon, instance) = two_shard_daemon("nfs-request-owner");
  let mut client = Client::connect(&instance);
  let port = daemon.nfs_port().unwrap();
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  stream.set_read_timeout(Some(CREDIT_WAIT)).unwrap();
  stream.set_write_timeout(Some(CREDIT_WAIT)).unwrap();
  // Shape: the two actual owner partitions, and two distinct ordinary Unix users.
  for partition in 0..2 {
    let name = (0..32)
      .map(|suffix| format!("ownership-{suffix}"))
      .find(|name| slates_server::verbs::owner_of_name(name, 2) == partition)
      .expect("both owner partitions have a fixture name");
    assert!(matches!(
      client.call(&scratch(&name)),
      ReplyBody::Created { .. }
    ));
    let root = mount(&mut stream, &capability_path(&daemon, &name), 1);
    let mut chmod = Vec::new();
    opaque(&root, &mut chmod);
    for word in [1_u32, 0o777, 0, 0, 0, 0, 0, 0] {
      chmod.extend_from_slice(&word.to_be_bytes());
    }
    assert_eq!(status(&call(&mut stream, NFS_PROGRAM, 2, &chmod, 2)), 0);
    for uid in [1001_u32, 1002] {
      assert_creation_as(&mut stream, &root, uid);
    }
  }
  daemon.stop();
}

/// The daemon serves a volume that lives on a shard OTHER than the one the NFS listener is on, over the
/// cross-shard bridge queue: the request is routed to the volume's owning shard, served there against
/// that shard's real state, and the reply routed back — a write over NFS reads back over NFS.
#[test]
fn the_daemon_serves_a_volume_on_another_shard_over_nfs() {
  let (daemon, instance) = two_shard_daemon("nfsxshard");
  let mut client = Client::connect(&instance);
  // The NFS listener is served on the control shard, whose partition is 0; a volume whose owner
  // partition is not 0 lives on the other shard and is reached over the cross-shard bridge queue.
  let control_partition = 0;

  let mut remote = None;
  for attempt in 0..32 {
    let ReplyBody::Created { id } = client.call(&scratch(&format!("vol-{attempt}"))) else {
      continue;
    };
    if slates_server::verbs::owner_of(id) != control_partition {
      remote = Some(format!("vol-{attempt}")); // the remote volume's friendly name
      break;
    }
  }
  let name = remote.expect("a volume provisioned on a non-control shard");

  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  // Mount the remote volume by its friendly name under its capability; the MNT routes across shards by
  // `owner_of_name`, and the capability is validated on the owning shard.
  let root_fh = mount(&mut stream, &capability_path(&daemon, &name), 1);
  let file_fh = create(&mut stream, &root_fh, "remote.txt", 2);
  let payload = b"served from a volume on another shard, over the cross-shard bridge queue\n";
  write(&mut stream, &file_fh, payload, 3);
  let got = read(&mut stream, &file_fh, 4);
  assert_eq!(
    got, payload,
    "a volume on another shard served over the cross-shard bridge queue, byte-for-byte"
  );

  drop(stream);
  drop(client);
  drop(daemon);
}

/// A client mounts the single host root `/` and reaches a volume on ANOTHER shard by `cd`-ing into it
/// by its friendly name (a root `LOOKUP` routed across shards by `owner_of_name`): the design's single
/// mount point under which every volume appears, reaching a remote volume, over NFS with no privilege.
#[test]
fn a_client_mounts_the_host_root_and_reaches_a_remote_volume_by_name() {
  let (daemon, instance) = two_shard_daemon("nfsrootname");
  let mut client = Client::connect(&instance);
  let control_partition = 0;

  // Provision a volume on a NON-control shard, deterministically: a create routes by `owner_of_name`
  // (verbs.rs), so we target only names whose owner is a non-control partition and create them until
  // one lands — instead of the old fixed `0..32` probe over arbitrary names, which flaked under load
  // (it silently `continue`d past a remote-owned create that came back non-`Created`, and could exhaust
  // its 32 tries). This skips control-owned names outright and surfaces the actual reply if every
  // remote-owned create is refused, so a real forwarding failure is a loud panic, not a silent give-up.
  let partitions = daemon.shards().len();
  let mut remote = None;
  let mut last_reply = None;
  for attempt in 0..256u32 {
    let candidate = format!("rv-{attempt}");
    if slates_server::verbs::owner_of_name(&candidate, partitions) == control_partition {
      continue; // a control-owned name; not what this test needs
    }
    match client.call(&scratch(&candidate)) {
      ReplyBody::Created { id } => {
        assert_eq!(
          slates_server::verbs::owner_of(id),
          slates_server::verbs::owner_of_name(&candidate, partitions),
          "a volume is created on its name's owner shard"
        );
        assert_ne!(
          slates_server::verbs::owner_of(id),
          control_partition,
          "and that owner is a non-control shard"
        );
        remote = Some(candidate);
        break;
      }
      other => last_reply = Some(format!("{other:?}")),
    }
  }
  let name = remote
    .unwrap_or_else(|| panic!("no remote volume created (last non-Created reply: {last_reply:?})"));

  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();

  // Mount the host root scoped to the remote volume's capability, then LOOKUP the volume by its name —
  // the root LOOKUP routes across shards by `owner_of_name`, and the owning shard resolves the name
  // against its own volumes and validates the capability the root handle carries (AUD-01).
  let root_fh = mount(&mut stream, &root_capability_path(&daemon, &name), 1);
  let volume_root = lookup(&mut stream, &root_fh, &name, 2);
  let file_fh = create(&mut stream, &volume_root, "byname.txt", 3);
  let payload = b"reached a remote volume by cd-ing into it from the single host root\n";
  write(&mut stream, &file_fh, payload, 4);
  let got = read(&mut stream, &file_fh, 5);
  assert_eq!(
    got, payload,
    "mounted the host root and reached a volume on another shard by its name, over NFS"
  );

  drop(stream);
  drop(client);
  drop(daemon);
}

/// The host root's listing gathers volumes from every shard over the bridge queue: a client mounts `/`,
/// lists it (READDIRPLUS), and sees a volume from the control shard AND a volume from another shard —
/// the design's single mount point under which every volume on the host appears.
#[test]
fn the_host_root_listing_gathers_volumes_from_every_shard() {
  let (daemon, instance) = two_shard_daemon("nfsrootlist");
  let mut client = Client::connect(&instance);
  let control_partition = 0;

  // Provision volumes until there is one on the control shard and one on another shard.
  let mut local = None;
  let mut remote = None;
  for attempt in 0..48 {
    let ReplyBody::Created { id } = client.call(&scratch(&format!("lv-{attempt}"))) else {
      continue;
    };
    if slates_server::verbs::owner_of(id) == control_partition {
      local.get_or_insert(format!("lv-{attempt}"));
    } else {
      remote.get_or_insert(format!("lv-{attempt}"));
    }
    if local.is_some() && remote.is_some() {
      break;
    }
  }
  let local = local.expect("a volume on the control shard");
  let remote = remote.expect("a volume on another shard");

  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  // The host root scoped to the other-shard volume's capability lists that volume — gathered over the
  // bridge queue from its owning shard — and nothing else (AUD-01: a listing shows only what the
  // presented capability authorizes); scoped to the control-shard volume's, only that one; a bare `/`
  // lists nothing.
  let remote_root = mount(&mut stream, &root_capability_path(&daemon, &remote), 1);
  let remote_names = readdirplus(&mut stream, &remote_root, 2);
  let local_root = mount(&mut stream, &root_capability_path(&daemon, &local), 3);
  let local_names = readdirplus(&mut stream, &local_root, 4);
  let bare_root = mount(&mut stream, "/", 5);
  let bare_names = readdirplus(&mut stream, &bare_root, 6);
  assert_eq!(
    remote_names,
    vec![remote.clone()],
    "the root scoped to the other-shard volume lists exactly it (gathered over the bridge queue)"
  );
  assert_eq!(
    local_names,
    vec![local.clone()],
    "the root scoped to the control-shard volume lists exactly it"
  );
  assert!(
    bare_names.is_empty(),
    "a bare `/` with no capability lists nothing: {bare_names:?}"
  );

  drop(stream);
  drop(client);
  drop(daemon);
}

/// AC-3.11 / T-3.14 (§4.6 "Writeback and snapshot barrier"; GAP-A9-4): a snapshot runs the barrier over
/// the shard's attachment registry and reports what it covers. Before any mount the volume has no live
/// attachment: the barrier closes none and the snapshot is **complete** (only ring writers, whose
/// writes are recorded before they return). Once the volume is mounted under its capability and written
/// through, the mount's requests ride a registry attachment: the barrier closes it (one attachment) and
/// the snapshot is **server-visible** — the NFS client may still hold acknowledged writes before its
/// `COMMIT`, which the reply must not claim. The mount keeps serving after the barrier (a write lands in
/// the next generation), and the kernel's `UMNT` ends the registry attachment with the catalog's: the
/// next snapshot closes none again. Non-vacuous: the closed count moves 0 → 1 → 0 with the mount's life.
#[test]
fn a_snapshot_over_a_mounted_volume_reports_the_barrier_it_closed() {
  let (daemon, instance) = single_shard_daemon("snapbarrier");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id: volume } = client.call(&scratch("vol")) else {
    panic!("the volume was not created");
  };
  let port = daemon.nfs_port().expect("the daemon is serving NFS");

  let before = snapshot_coverage(&mut client, volume);

  let path = capability_path(&daemon, "vol");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = mount(&mut stream, &path, 1);
  let file = create(&mut stream, &root, "f", 2);
  write(&mut stream, &file, b"before the snapshot\n", 3);
  let mounted = snapshot_coverage(&mut client, volume);
  // The mount serves on: a write after the barrier belongs to the next generation (longer than the
  // first, so the whole file is the new bytes — a write at offset zero truncates nothing).
  let after = b"after the snapshot: the next generation\n";
  write(&mut stream, &file, after, 4);
  assert_eq!(read(&mut stream, &file, 5), after);

  umnt(&mut stream, &path, 6);
  let unmounted = snapshot_coverage(&mut client, volume);
  daemon.stop();

  assert_eq!(
    before,
    (SnapshotBoundary::Complete, 0),
    "no mount: the barrier closes nothing and the snapshot is complete"
  );
  assert_eq!(
    mounted,
    (SnapshotBoundary::ServerVisible, 1),
    "the mount's attachment is closed by the barrier, and an NFS client may hold acknowledged writes"
  );
  assert_eq!(
    unmounted,
    (SnapshotBoundary::Complete, 0),
    "the kernel's UMNT ended the registry attachment with the catalog's"
  );
}

/// AC (§4.4 `Binding → Bound`; GAP-A9-4): a host mount's attachment is bound to the mount point the
/// mounting process established, and the volume's status reports it. The owner attaches as a host
/// mount and binds the path: `status` lists that mount with its attachment. A consumer that is not the
/// attachment's principal cannot bind it (`Forbidden`); an SDK attachment, which establishes no host
/// mount, cannot be bound (`BadRequest`); a `detach` ends the mount and the status lists it no more.
/// Non-vacuous: the listing moves empty → one → empty with the attachment's life.
#[test]
fn a_host_mount_binds_its_mount_point_and_the_status_reports_it() {
  // Two shards, and a volume the control shard does not own: the bind, the status and the detach all
  // route to the attachment's owner over the cross-shard path, as `slates mount` on a real daemon does.
  let (daemon, instance) = two_shard_daemon("bindmount");
  let secret = daemon.segment().issuer_secret().unwrap();
  let account = rustix::process::getuid().as_raw();
  let mut owner = Client::connect(&instance);
  let mut other = Client::connect(&instance);
  enroll_consumer(&mut owner, &mut other, &secret, account);
  let volume = remote_volume(&mut owner);
  let before = mounts_reported(&mut owner, volume);

  let (attachment, _token) = attach_host_mount(&mut owner, volume);
  let path = "/Users/ada/projects/vol".to_owned();
  let bound = matches!(
    owner.call(&RequestBody::BindMount {
      attachment,
      path: path.clone(),
    }),
    ReplyBody::MountBound
  );
  let listed = mounts_reported(&mut owner, volume);
  // Another principal cannot bind the owner's attachment; an SDK attachment has no mount point.
  let (forged, unbound) = bind_refusals(&mut other, &mut owner, attachment, volume);
  let detached = matches!(
    owner.call(&RequestBody::Detach { attachment }),
    ReplyBody::Detached
  );
  let after = mounts_reported(&mut owner, volume);
  daemon.stop();

  assert!(before.is_empty(), "nothing is bound before a mount");
  assert!(bound, "the owner binds its mount");
  assert_eq!(
    listed,
    vec![(attachment, path)],
    "the status lists the bound mount with its attachment"
  );
  assert!(
    refused_as(&forged, RefusalKind::Forbidden),
    "another principal is refused: {forged:?}"
  );
  assert!(
    refused_as(&unbound, RefusalKind::BadRequest),
    "an SDK attachment binds no mount point: {unbound:?}"
  );
  assert!(detached, "the owner detaches its mount");
  assert!(after.is_empty(), "a detached mount is listed no more");
}

/// Creates volumes until one lands on a partition other than the control shard's (0), and returns it:
/// a volume every attachment-routed verb reaches over the cross-shard path.
fn remote_volume(client: &mut Client) -> VolumeId {
  for attempt in 0..32 {
    let ReplyBody::Created { id } = client.call(&scratch(&format!("vol-{attempt}"))) else {
      continue;
    };
    if slates_server::verbs::owner_of(id) != 0 {
      return id;
    }
  }
  panic!("no volume landed on the non-control shard in 32 attempts");
}

/// The refusal kinds the bind test tells apart.
#[derive(Clone, Copy)]
enum RefusalKind {
  Forbidden,
  BadRequest,
}

/// Whether `reply` is a refusal of `kind`.
fn refused_as(reply: &ReplyBody, kind: RefusalKind) -> bool {
  use slates_ipc::protocol::Refusal;
  match kind {
    RefusalKind::Forbidden => matches!(
      reply,
      ReplyBody::Refused {
        refusal: Refusal::Forbidden { .. }
      }
    ),
    RefusalKind::BadRequest => matches!(
      reply,
      ReplyBody::Refused {
        refusal: Refusal::BadRequest { .. }
      }
    ),
  }
}

/// The two binds that must be refused: `other` (not the attachment's principal) binding the owner's
/// host-mount `attachment`, and the owner binding an SDK attachment (the record form, which establishes
/// no host mount). Returns the two replies.
fn bind_refusals(
  other: &mut Client,
  owner: &mut Client,
  attachment: u64,
  volume: VolumeId,
) -> (ReplyBody, ReplyBody) {
  let forged = other.call(&RequestBody::BindMount {
    attachment,
    path: "/elsewhere".to_owned(),
  });
  let ReplyBody::Attached {
    attachment: sdk, ..
  } = owner.call(&RequestBody::Attach {
    volume,
    snapshot: None,
    intent: Intent::Read,
    form: AttachRequest::Root,
  })
  else {
    panic!("the SDK attachment was refused");
  };
  let unbound = owner.call(&RequestBody::BindMount {
    attachment: sdk,
    path: "/elsewhere".to_owned(),
  });
  (forged, unbound)
}

/// The bound mounts a volume's status reports, as (attachment, path) pairs.
fn mounts_reported(client: &mut Client, volume: VolumeId) -> Vec<(u64, String)> {
  match client.call(&RequestBody::Status { volume }) {
    ReplyBody::Status { report } => report
      .mounts
      .iter()
      .map(|mount| (mount.attachment, mount.path.clone()))
      .collect(),
    other => panic!("the status was refused: {other:?}"),
  }
}

/// Takes a snapshot of `volume` through the client and returns the barrier's account: the boundary and
/// the attachments it closed.
fn snapshot_coverage(client: &mut Client, volume: VolumeId) -> (SnapshotBoundary, u32) {
  match client.call(&RequestBody::Snapshot { volume }) {
    ReplyBody::Snapshotted { coverage, .. } => (coverage.boundary, coverage.attachments_closed),
    other => panic!("the snapshot was refused: {other:?}"),
  }
}

/// The NFS mount path carrying an owner mount capability for the volume named `name` on `daemon`
/// (§4.13; AUD-01): `/<name>@<attachment_hex>.<token_hex>`.
fn capability_path(daemon: &Daemon, name: &str) -> String {
  daemon
    .mount_capability(name)
    .expect("the name's owner shard answers")
    .expect("a volume by that name is provisioned there")
}

/// The host-root mount path scoped to the capability of the volume named `name`: `/@<attachment>.<token>`.
fn root_capability_path(daemon: &Daemon, name: &str) -> String {
  let path = capability_path(daemon, name);
  let (_, capability) = path.rsplit_once('@').expect("a capability path");
  format!("/@{capability}")
}

/// AC (§4.13; AUD-01): a volume private to an enrolled consumer is served over NFS only through the
/// consumer's mount capability. An unbound TCP client (no token), a forged `AUTH_SYS` uid and a wrong
/// token are all refused at `MNT`, and the private volume is hidden from an unbound `ls /`; the mount
/// presenting the attachment's token is served, a file written through it reads back on a connection
/// that presented no token (the handle carries the capability), and the root scoped to the capability
/// lists exactly that volume. The mount's attachment is the mount's: it holds the write lease, survives
/// a `UMNT` of the scoped root, and ends with the kernel's `UMNT` of the mount path — the handle refused,
/// the attachment gone, the lease released. Non-vacuous: this same private volume serves once the
/// capability is presented, and stops once the mount is unmounted.
#[test]
fn a_consumer_private_volume_is_served_over_nfs_only_through_its_attachment_capability() {
  let (daemon, instance) = single_shard_daemon("aud01");
  let secret = daemon.segment().issuer_secret().unwrap();
  let account = rustix::process::getuid().as_raw();
  let mut owner = Client::connect(&instance); // the human surface: enrolls the consumer
  let mut workload = Client::connect(&instance); // the consumer's own channel
  enroll_consumer(&mut owner, &mut workload, &secret, account);

  // The consumer creates a volume it owns — private to it (owner is a `Consumer` principal).
  let ReplyBody::Created { id: private } = workload.call(&scratch("private")) else {
    panic!("the consumer's volume was not created");
  };
  assert!(
    matches!(
      owner.call(&RequestBody::Status { volume: private }),
      ReplyBody::Refused {
        refusal: slates_ipc::protocol::Refusal::Forbidden { .. }
      }
    ),
    "the volume is private to the consumer even from the account's own uid channel"
  );

  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let (attachment, token) = attach_host_mount(&mut workload, private);
  let capability_path = format!("/private@{attachment:x}.{}", hex16(&token));

  let Unauthorized {
    refused_no_token,
    listed_unbound,
    refused_forged_uid,
    refused_wrong_token,
  } = unauthorized_probes(port, account, attachment);
  let payload = b"through the consumer's capability\n";
  let Authorized {
    got,
    listed: listed_authorized,
    file,
  } = authorized_round_trip(port, &capability_path, payload);
  let unmounted = unmount_lifetime(port, &mut workload, private, &capability_path, &file);

  daemon.stop();
  assert_unmounted(&unmounted);
  assert_eq!(
    refused_no_token, MNT3ERR_NOENT,
    "an unbound TCP client cannot mount the consumer-private volume"
  );
  assert!(
    listed_unbound.is_empty(),
    "an unbound `ls /` lists nothing, the consumer-private volume least of all: {listed_unbound:?}"
  );
  assert_eq!(
    refused_forged_uid, MNT3ERR_NOENT,
    "a forged AUTH_SYS uid cannot mount the consumer-private volume"
  );
  assert_eq!(
    refused_wrong_token, MNT3ERR_NOENT,
    "a wrong capability token cannot mount the consumer-private volume"
  );
  assert_eq!(
    got, payload,
    "reads and writes go through the capability mount, the read on a connection that presented no token"
  );
  assert_eq!(
    listed_authorized,
    vec!["private".to_owned()],
    "the root scoped to the capability lists exactly the consumer's volume"
  );
}

/// Enrolls a consumer through the human surface (`owner`, proving the anchor's issuer secret) and binds
/// the `workload` channel to it with the enrollment's capability (§4.13).
fn enroll_consumer(
  owner: &mut Client,
  workload: &mut Client,
  secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES],
  account: u32,
) {
  let ReplyBody::Enrolled {
    consumer,
    secret: capability,
  } = owner.call(&RequestBody::Enroll {
    account,
    proof: slates_server::landing::enroll_proof(secret, account),
  })
  else {
    panic!("the human surface's enrollment was refused");
  };
  let proof = slates_server::landing::attest_proof(&capability, workload.client);
  assert!(
    matches!(
      workload.call(&RequestBody::Attest { consumer, proof }),
      ReplyBody::Attested
    ),
    "the genuine capability binds the workload channel to the consumer"
  );
}

/// The consumer attaches `volume` for a writable host mount (§4.6, §4.13) and receives the attachment id
/// and the capability token the mount presents.
fn attach_host_mount(workload: &mut Client, volume: VolumeId) -> (u64, [u8; 16]) {
  let ReplyBody::Attached {
    attachment,
    token: Some(token),
    ..
  } = workload.call(&RequestBody::Attach {
    volume,
    snapshot: None,
    intent: Intent::Write,
    form: AttachRequest::HostMount,
  })
  else {
    panic!("the consumer's host-mount attachment was refused");
  };
  (attachment, token)
}

/// What every caller without the consumer's capability observes (AUD-01).
struct Unauthorized {
  /// The `MNT` status an unbound TCP client (no capability) gets for the private volume.
  refused_no_token: u32,
  /// What an unbound `ls /` lists.
  listed_unbound: Vec<String>,
  /// The `MNT` status a caller forging the account's uid in `AUTH_SYS` gets.
  refused_forged_uid: u32,
  /// The `MNT` status a caller presenting the right attachment id with a wrong token gets.
  refused_wrong_token: u32,
}

/// Probes the private volume as an unbound TCP client, as a caller forging the account's uid, and as a
/// caller with a wrong token for the real attachment id — none of which holds the capability.
fn unauthorized_probes(port: u16, account: u32, attachment: u64) -> Unauthorized {
  let mut unbound = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let refused_no_token = mount_status(&mut unbound, "/private", 1);
  let root_fh = mount(&mut unbound, "/", 2);
  let listed_unbound = readdirplus(&mut unbound, &root_fh, 3);
  let refused_forged_uid = mount_status_as(&mut unbound, "/private", account, 4);
  let mut wrong = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let refused_wrong_token = mount_status(
    &mut wrong,
    &format!("/private@{attachment:x}.{}", hex16(&[0x11; 16])),
    1,
  );
  Unauthorized {
    refused_no_token,
    listed_unbound,
    refused_forged_uid,
    refused_wrong_token,
  }
}

/// What the consumer's capability serves (AUD-01).
struct Authorized {
  /// The bytes read back through the mount's handle on a connection that presented no token.
  got: Vec<u8>,
  /// What the host root scoped to the capability lists.
  listed: Vec<String>,
  /// The file's handle, carrying the capability — what a kernel keeps across the mount's life.
  file: Vec<u8>,
}

/// Mounts the private volume under its capability, writes `payload` into `f`, reads it back on a
/// **second connection that never presented the token in a path** (the handle alone authorizes it: the
/// capability rides in the handle, robust across a mount's connection topology), and lists the host root
/// scoped to the capability.
fn authorized_round_trip(port: u16, capability_path: &str, payload: &[u8]) -> Authorized {
  let mut authorized = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = mount(&mut authorized, capability_path, 1);
  let file = create(&mut authorized, &root, "f", 2);
  write(&mut authorized, &file, payload, 3);
  let mut other_connection = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let got = read(&mut other_connection, &file, 1);
  let (_, capability) = capability_path.rsplit_once('@').expect("a capability path");
  let host_root = mount(&mut authorized, &format!("/@{capability}"), 4);
  let listed = readdirplus(&mut authorized, &host_root, 5);
  Authorized { got, listed, file }
}

/// How the mount's attachment ends (AUD-01): with the kernel's `UMNT` of the mount path, not before.
struct Unmounted {
  /// The volume's attachment count and write-lease epoch while mounted.
  attachments_mounted: u32,
  lease_mounted: Option<u64>,
  /// The READ status through the mount's handle after a `UMNT` of the capability-scoped root.
  read_after_root_umnt: u32,
  /// The READ status through the mount's handle after the `UMNT` of the volume's mount path.
  read_after_umnt: u32,
  /// The volume's attachment count and lease after that `UMNT`.
  attachments_after_umnt: u32,
  lease_after_umnt: Option<u64>,
}

/// The volume's attachment count and write-lease epoch, as its consumer's `status` reports them.
fn attachments_and_lease(consumer: &mut Client, volume: VolumeId) -> (u32, Option<u64>) {
  let ReplyBody::Status { report } = consumer.call(&RequestBody::Status { volume }) else {
    panic!("the consumer's status of its own volume was refused");
  };
  (report.attachments, report.lease_epoch)
}

/// Sends the kernel's unmount signals the way `umount` does — first for the capability-scoped root
/// (a browse, which ends nothing), then for the volume's mount path (which ends the attachment) — and
/// reads through the mount's handle after each, with the volume's status around them.
fn unmount_lifetime(
  port: u16,
  consumer: &mut Client,
  volume: VolumeId,
  capability_path: &str,
  file: &[u8],
) -> Unmounted {
  let (attachments_mounted, lease_mounted) = attachments_and_lease(consumer, volume);
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let (_, capability) = capability_path.rsplit_once('@').expect("a capability path");
  umnt(&mut stream, &format!("/@{capability}"), 1);
  let read_after_root_umnt = read_status(&mut stream, file, 2);
  umnt(&mut stream, capability_path, 3);
  let read_after_umnt = read_status(&mut stream, file, 4);
  let (attachments_after_umnt, lease_after_umnt) = attachments_and_lease(consumer, volume);
  Unmounted {
    attachments_mounted,
    lease_mounted,
    read_after_root_umnt,
    read_after_umnt,
    attachments_after_umnt,
    lease_after_umnt,
  }
}

/// The mount's attachment is the mount's: one attachment holding the write lease while mounted, still
/// serving after the root browse is unmounted, and ended — the handle refused, the attachment gone, the
/// lease released — by the `UMNT` of the mount path.
fn assert_unmounted(unmounted: &Unmounted) {
  assert_eq!(
    unmounted.attachments_mounted, 1,
    "the mount is the volume's one attachment"
  );
  assert!(
    unmounted.lease_mounted.is_some(),
    "a write mount holds the volume's write lease (D-16)"
  );
  assert_eq!(
    unmounted.read_after_root_umnt, 0,
    "a UMNT of the capability-scoped root ends nothing: the mount's handle still serves"
  );
  assert_eq!(
    unmounted.read_after_umnt, NFS3ERR_ACCES,
    "after the UMNT of the mount path the handle's capability authorizes nothing"
  );
  assert_eq!(
    unmounted.attachments_after_umnt, 0,
    "the kernel's UMNT ended the mount's attachment"
  );
  assert_eq!(
    unmounted.lease_after_umnt, None,
    "the holder's last write attachment released the lease"
  );
}

/// The `MNT` status of `path` on `stream` under an `AUTH_SYS` credential claiming `uid` (no groups),
/// without asserting success — the forged-uid probe of the authorization gate (AUD-01).
fn mount_status_as(stream: &mut TcpStream, path: &str, uid: u32, xid: u32) -> u32 {
  let mut args = Vec::new();
  opaque(path.as_bytes(), &mut args);
  status(&call_as(stream, MOUNT_PROGRAM, 1, &args, xid, uid))
}

/// One RPC call under an `AUTH_SYS` credential (RFC 5531 §8.2: stamp, machine name, uid, gid, no
/// supplementary gids) claiming `uid`, returning the accepted reply's result bytes. The credential is
/// client-supplied — exactly why it authorizes nothing at the mount edge.
fn call_as(
  stream: &mut TcpStream,
  program: u32,
  procedure: u32,
  args: &[u8],
  xid: u32,
  uid: u32,
) -> Vec<u8> {
  use std::io::{Read, Write};
  let mut credential = Vec::new();
  credential.extend_from_slice(&0u32.to_be_bytes()); // stamp
  credential.extend_from_slice(&0u32.to_be_bytes()); // machine name: empty
  credential.extend_from_slice(&uid.to_be_bytes());
  credential.extend_from_slice(&uid.to_be_bytes()); // gid
  credential.extend_from_slice(&0u32.to_be_bytes()); // no supplementary gids
  let mut body = Vec::new();
  for field in [xid, 0, 2, program, 3, procedure, 1 /* AUTH_SYS */] {
    body.extend_from_slice(&field.to_be_bytes());
  }
  body.extend_from_slice(&u32::try_from(credential.len()).unwrap().to_be_bytes());
  body.extend_from_slice(&credential);
  body.extend_from_slice(&0u32.to_be_bytes()); // verifier: AUTH_NONE
  body.extend_from_slice(&0u32.to_be_bytes());
  body.extend_from_slice(args);
  let marker = 0x8000_0000u32 | u32::try_from(body.len()).unwrap();
  stream.write_all(&marker.to_be_bytes()).unwrap();
  stream.write_all(&body).unwrap();
  let mut marker_buf = [0u8; 4];
  stream.read_exact(&mut marker_buf).unwrap();
  let len = (u32::from_be_bytes(marker_buf) & 0x7fff_ffff) as usize;
  let mut reply = vec![0u8; len];
  stream.read_exact(&mut reply).unwrap();
  let verf_len = u32::from_be_bytes(reply[16..20].try_into().unwrap()) as usize;
  let accept_off = 20 + verf_len + (4 - verf_len % 4) % 4;
  assert_eq!(
    u32::from_be_bytes(reply[accept_off..accept_off + 4].try_into().unwrap()),
    0,
    "RPC accepted"
  );
  reply[accept_off + 4..].to_vec()
}

/// The `MNT` status of `path` on `stream`, without asserting success — so a refused mount (a
/// consumer-private volume without the capability, AUD-01) is observed, not panicked.
fn mount_status(stream: &mut TcpStream, path: &str, xid: u32) -> u32 {
  let mut args = Vec::new();
  opaque(path.as_bytes(), &mut args);
  status(&call(stream, MOUNT_PROGRAM, 1, &args, xid))
}

/// Sixteen bytes as 32 lowercase hex digits — the mount capability token in a capability mount path.
fn hex16(bytes: &[u8; 16]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}
