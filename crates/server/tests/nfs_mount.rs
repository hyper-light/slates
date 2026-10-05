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
use slates_server::{Daemon, DaemonConfig, SegmentSource};
use slates_wire::request::RequestId;

mod common;
use common::nfs::{
  MOUNT_PROGRAM, call, create, lookup, mount, opaque, read, read_status, readdirplus, status, umnt,
  write,
};

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
  two_shard_daemon_with(name, |_| {})
}

/// [`two_shard_daemon`] with its derived configuration adjusted by `adjust` before it starts.
fn two_shard_daemon_with(name: &str, adjust: impl FnOnce(&mut DaemonConfig)) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  // Two shards, so a volume can land on a shard other than the one the NFS listener is served on,
  // exercising the cross-shard bridge queue.
  let mut config = DaemonConfig::derive(&profile, &instance, Some(2));
  adjust(&mut config);
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

/// Shape: the calls a mount makes after its `MNT` in the connection tests: enough that a per-call forward would
/// dominate the counts.
const CALLS_AFTER_MOUNT: u32 = 24;

/// A provisioned volume's friendly name whose owner partition is (`remote`) or is not the control shard's.
fn volume_named_on(client: &mut Client, remote: bool, prefix: &str) -> String {
  for attempt in 0..32 {
    let name = format!("{prefix}-{attempt}");
    let ReplyBody::Created { id } = client.call(&scratch(&name)) else {
      continue;
    };
    if (slates_server::verbs::owner_of(id) != 0) == remote {
      return name;
    }
  }
  panic!("no volume landed where wanted in 32 attempts");
}

/// §4.6 (connection affinity, 2026-10-04): do mount a volume another shard owns over one connection and make a
/// burst of calls; expect the bytes to round-trip and every call after the `MNT` served on the owner shard (the
/// connection moved there at its first call), none forwarded over the bridge queue. The `MNT` itself routes by
/// name, so it is the one forwarded call. The service counts are the non-vacuity evidence: a silently dead move
/// would show every call forwarded, as all 185,745 were before it.
#[test]
fn a_mounts_connection_moves_to_its_volumes_owner_and_is_served_there() {
  let (daemon, instance) = two_shard_daemon("nfsmove");
  let mut client = Client::connect(&instance);
  let name = volume_named_on(&mut client, true, "moved");
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root_fh = mount(&mut stream, &capability_path(&daemon, &name), 1);
  let file_fh = create(&mut stream, &root_fh, "moved.txt", 2);
  let payload = b"served on the owner shard after the connection moved\n";
  let mut xid = 3;
  for _ in 0..CALLS_AFTER_MOUNT / 2 {
    write(&mut stream, &file_fh, payload, xid);
    assert_eq!(
      read(&mut stream, &file_fh, xid + 1),
      payload,
      "the bytes round-trip"
    );
    xid += 2;
  }
  let times = daemon.nfs_service_times().unwrap();
  let (local, forwarded) = (times.local, times.forwarded);
  drop(stream);
  drop(client);
  drop(daemon);
  assert_eq!(
    forwarded.count, 1,
    "only the MNT was forwarded; every later call was served on the owner"
  );
  assert!(
    local.count > u64::from(CALLS_AFTER_MOUNT),
    "the calls after the MNT were served where the connection moved ({} local)",
    local.count
  );
}

/// §4.6 (connection affinity): do alternate one connection's calls between a volume the control shard owns and one
/// the other shard owns; expect each volume's bytes intact on every call, the connection following each call to
/// its owner (a move costs one hop where a forward cost two), and none forwarded after the two `MNT`s.
#[test]
fn a_connection_alternating_between_volumes_on_two_shards_serves_both() {
  let (daemon, instance) = two_shard_daemon("nfsalternate");
  let mut client = Client::connect(&instance);
  let here = volume_named_on(&mut client, false, "here");
  let there = volume_named_on(&mut client, true, "there");
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let here_root = mount(&mut stream, &capability_path(&daemon, &here), 1);
  let there_root = mount(&mut stream, &capability_path(&daemon, &there), 2);
  let here_file = create(&mut stream, &here_root, "here.txt", 3);
  let there_file = create(&mut stream, &there_root, "there.txt", 4);
  let mut xid = 5;
  for round in 0..CALLS_AFTER_MOUNT / 4 {
    let here_bytes = format!("here {round}\n").into_bytes();
    let there_bytes = format!("there {round}\n").into_bytes();
    write(&mut stream, &here_file, &here_bytes, xid);
    write(&mut stream, &there_file, &there_bytes, xid + 1);
    assert_eq!(
      read(&mut stream, &here_file, xid + 2),
      here_bytes,
      "the control shard's volume"
    );
    assert_eq!(
      read(&mut stream, &there_file, xid + 3),
      there_bytes,
      "the other shard's volume"
    );
    xid += 4;
  }
  let forwarded = daemon.nfs_service_times().unwrap().forwarded;
  drop(stream);
  drop(client);
  drop(daemon);
  assert!(
    forwarded.count <= 2,
    "at most the two MNTs were forwarded ({} were)",
    forwarded.count
  );
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
/// the next generation), and the mount's end — its bound mount point confirmed empty after the
/// kernel's `UMNT` on macOS, a `detach` elsewhere (§4.6 A-34) — ends the registry attachment with the
/// catalog's: the next snapshot closes none again. Non-vacuous: the closed count moves 0 → 1 → 0 with the mount's life.
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

  let (name, capability) = path.rsplit_once('@').expect("a capability path");
  end_host_mount(
    &mut client,
    &mut stream,
    volume,
    name,
    attachment_of(capability),
  );
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
    "the mount's end removed the registry attachment with the catalog's"
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
/// a `UMNT` of the scoped root and a `UMNT` presenting the capability, and ends the platform's way
/// (§4.6 A-34: the kernel's `UMNT` of `/<name>` with the bound mount point confirmed empty on macOS, a
/// `detach` elsewhere) — the handle refused, the attachment gone, the lease released. Non-vacuous: this
/// same private volume serves once the capability is presented, and stops once the mount has ended.
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
  /// The READ status through the mount's handle after a `UMNT` that presents the capability in its
  /// path (not the kernel's form since A-34: the mount source carries none).
  read_after_capability_umnt: u32,
  /// The READ status through the mount's handle once the mount has ended the platform's way.
  read_after_end: u32,
  /// The volume's attachment count and lease after the end.
  attachments_after_end: u32,
  lease_after_end: Option<u64>,
}

/// The volume's attachment count and write-lease epoch, as its consumer's `status` reports them.
fn attachments_and_lease(consumer: &mut Client, volume: VolumeId) -> (u32, Option<u64>) {
  let ReplyBody::Status { report } = consumer.call(&RequestBody::Status { volume }) else {
    panic!("the consumer's status of its own volume was refused");
  };
  (report.attachments, report.lease_epoch)
}

/// Sends the unmount signals a client can send — a `UMNT` of the capability-scoped root (a browse) and
/// one presenting the capability in its path, neither of which ends anything (§4.6 A-34) — then ends
/// the mount the platform's way ([`end_host_mount`]), reading through the mount's handle after each,
/// with the volume's status around them.
fn unmount_lifetime(
  port: u16,
  consumer: &mut Client,
  volume: VolumeId,
  capability_path: &str,
  file: &[u8],
) -> Unmounted {
  let (attachments_mounted, lease_mounted) = attachments_and_lease(consumer, volume);
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let (name, capability) = capability_path.rsplit_once('@').expect("a capability path");
  umnt(&mut stream, &format!("/@{capability}"), 1);
  let read_after_root_umnt = read_status(&mut stream, file, 2);
  umnt(&mut stream, capability_path, 3);
  let read_after_capability_umnt = read_status(&mut stream, file, 4);
  let attachment = attachment_of(capability);
  end_host_mount(consumer, &mut stream, volume, name, attachment);
  let read_after_end = read_status(&mut stream, file, 5);
  let (attachments_after_end, lease_after_end) = attachments_and_lease(consumer, volume);
  Unmounted {
    attachments_mounted,
    lease_mounted,
    read_after_root_umnt,
    read_after_capability_umnt,
    read_after_end,
    attachments_after_end,
    lease_after_end,
  }
}

/// The attachment id a capability `<attachment_hex>.<token_hex>` names.
fn attachment_of(capability: &str) -> u64 {
  let (attachment, _) = capability.split_once('.').expect("a capability");
  u64::from_str_radix(attachment, 16).expect("a hexadecimal attachment id")
}

/// Ends a host mount's `attachment` the platform's way (§4.6 A-34). The attachment is bound to a mount
/// point that holds no mount (a path this test never creates), and the kernel's `UMNT` of `/<name>` —
/// the only form the kernel sends, the mount source carrying no capability — is sent. On macOS the
/// daemon confirms against the kernel's mount table that the point no longer holds the mount, and the
/// attachment ends. Elsewhere a `UMNT` is never proof of an unmount (a FUSE mount ends when the kernel
/// closes the device), so the attachment still serves and is ended by a `detach`.
fn end_host_mount(
  owner: &mut Client,
  stream: &mut TcpStream,
  volume: VolumeId,
  name: &str,
  attachment: u64,
) {
  let mount_point = format!("/nonexistent-slates-test-{}/mnt", std::process::id());
  assert!(
    matches!(
      owner.call(&RequestBody::BindMount {
        attachment,
        path: mount_point,
      }),
      ReplyBody::MountBound
    ),
    "the owner binds its mount's mount point"
  );
  let (attachments, _) = attachments_and_lease(owner, volume);
  umnt(stream, name, 90);
  if cfg!(target_os = "macos") {
    let started = Instant::now();
    while attachments_and_lease(owner, volume).0 >= attachments {
      assert!(
        started.elapsed() < CREDIT_WAIT,
        "a UMNT whose bound mount point holds no mount ends its attachment"
      );
      std::thread::yield_now();
    }
  } else {
    assert_eq!(
      attachments_and_lease(owner, volume).0,
      attachments,
      "off macOS a UMNT alone ends nothing"
    );
    assert!(
      matches!(
        owner.call(&RequestBody::Detach { attachment }),
        ReplyBody::Detached
      ),
      "the owner detaches its mount"
    );
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
    unmounted.read_after_capability_umnt, 0,
    "a UMNT presenting the capability is not the kernel's form and ends nothing (A-34)"
  );
  assert_eq!(
    unmounted.read_after_end, NFS3ERR_ACCES,
    "once the mount has ended the handle's capability authorizes nothing"
  );
  assert_eq!(
    unmounted.attachments_after_end, 0,
    "the end removed the mount's attachment"
  );
  assert_eq!(
    unmounted.lease_after_end, None,
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

// --- NFSv4.1/4.2 over the daemon's listener (§4.6 A-35). ---

/// A hand-rolled NFSv4 client over one connection: its session, and slot 0's sequence.
struct V4Client {
  stream: TcpStream,
  clientid: u64,
  sessionid: [u8; 16],
  sequence: u32,
  xid: u32,
  /// The slots the server granted the session (`ca_maxrequests` of the fore channel).
  slots: u32,
  /// Whether the server granted the back channel on this connection (`CREATE_SESSION4_FLAG_CONN_BACK_CHAN`).
  back_granted: bool,
  /// The major id of the `server_owner4` EXCHANGE_ID answered with.
  server_owner: Vec<u8>,
}

/// Format: `NFS4_OK`, `NFS4ERR_NOENT` and the operation numbers this test sends (RFC 7863).
const NFS4_OK: u32 = 0;
const NFS4ERR_NOENT: u32 = 2;
const OP_CLOSE: u32 = 4;
const OP_GETFH: u32 = 10;
const OP_LOOKUP: u32 = 15;
const OP_OPEN: u32 = 18;
const OP_PUTFH: u32 = 22;
const OP_PUTROOTFH: u32 = 24;
const OP_READ: u32 = 25;
const OP_WRITE: u32 = 38;
const OP_EXCHANGE_ID: u32 = 42;
const OP_CREATE_SESSION: u32 = 43;
const OP_SEQUENCE: u32 = 53;
/// Format: `OP_ACCESS` and `OP_DESTROY_SESSION` (RFC 7863).
const OP_ACCESS: u32 = 3;
const OP_DESTROY_SESSION: u32 = 44;

impl V4Client {
  /// One ONC RPC call of NFS version `version` (AUTH_SYS as uid 0): the accept status and the rest.
  fn rpc(&mut self, version: u32, procedure: u32, args: &[u8]) -> (u32, Vec<u8>) {
    use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
    use std::io::{Read, Write};
    self.xid += 1;
    let mut body = XdrWriter::new();
    for field in [self.xid, 0, 2, 100_003, version, procedure, 0, 0, 0, 0] {
      body.u32(field);
    }
    body.fixed(args);
    let marker = 0x8000_0000u32 | u32::try_from(body.len()).unwrap();
    self.stream.write_all(&marker.to_be_bytes()).unwrap();
    self.stream.write_all(body.as_slice()).unwrap();
    let mut marker = [0u8; 4];
    self.stream.read_exact(&mut marker).unwrap();
    let mut reply = vec![0u8; (u32::from_be_bytes(marker) & 0x7fff_ffff) as usize];
    self.stream.read_exact(&mut reply).unwrap();
    let mut reader = XdrReader::new(&reply);
    reader.fixed(12).unwrap();
    reader.u32().unwrap();
    reader.opaque(400).unwrap();
    let accept = reader.u32().unwrap();
    (accept, reader.rest().to_vec())
  }

  /// A COMPOUND (minor version 2) of `ops` already encoded: the reply's status and its results after
  /// the frame.
  fn compound(&mut self, count: u32, ops: &[u8]) -> (u32, Vec<u8>) {
    use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
    let mut args = XdrWriter::new();
    args.opaque(b"");
    args.u32(2);
    args.u32(count);
    args.fixed(ops);
    let (accept, reply) = self.rpc(4, 1, args.as_slice());
    assert_eq!(accept, 0, "the COMPOUND is accepted");
    let mut reader = XdrReader::new(&reply);
    let status = reader.u32().unwrap();
    reader.opaque(1024).unwrap();
    reader.u32().unwrap();
    (status, reader.rest().to_vec())
  }

  /// EXCHANGE_ID and CREATE_SESSION over a new connection to `port`.
  fn connect(port: u16) -> V4Client {
    V4Client::connect_as(port, b"v4-test-host")
  }

  /// EXCHANGE_ID for the client owner `owner`, then CREATE_SESSION, over a new connection to `port`.
  fn connect_as(port: u16, owner: &[u8]) -> V4Client {
    V4Client::connect_asking(port, owner, 4)
  }

  /// [`V4Client::connect_as`] asking for `slots` slots, as a client's `max_session_slots` does.
  fn connect_asking(port: u16, owner: &[u8], slots: u32) -> V4Client {
    V4Client::connect_full(port, owner, slots, None)
  }

  /// [`V4Client::connect_asking`], asking for the back channel on this connection to callback `program`.
  fn connect_full(port: u16, owner: &[u8], slots: u32, back: Option<u32>) -> V4Client {
    use slates_bridge_nfs::v4::types::ChannelAttrs;
    use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
    let mut client = V4Client {
      stream: TcpStream::connect(("127.0.0.1", port)).unwrap(),
      clientid: 0,
      sessionid: [0; 16],
      sequence: 0,
      xid: 0,
      slots: 0,
      back_granted: false,
      server_owner: Vec::new(),
    };
    let mut ops = XdrWriter::new();
    ops.u32(OP_EXCHANGE_ID);
    ops.fixed(&[1; 8]);
    ops.opaque(owner);
    ops.u32(0);
    ops.u32(0);
    ops.u32(0);
    let (status, results) = client.compound(1, ops.as_slice());
    assert_eq!(status, NFS4_OK, "EXCHANGE_ID");
    let mut reader = XdrReader::new(&results);
    reader.fixed(8).unwrap();
    let clientid = reader.u64().unwrap();
    client.clientid = clientid;
    let sequenceid = reader.u32().unwrap();
    reader.u32().unwrap(); // flags
    reader.u32().unwrap(); // SP4_NONE
    reader.u64().unwrap(); // so_minor_id
    client.server_owner = reader.opaque(1024).unwrap().to_vec();
    let mut ops = XdrWriter::new();
    ops.u32(OP_CREATE_SESSION);
    ops.u64(clientid);
    ops.u32(sequenceid);
    ops.u32(if back.is_some() { CONN_BACK_CHAN } else { 0 });
    let asked = ChannelAttrs {
      max_request: 1 << 20,
      max_response: 1 << 20,
      max_response_cached: 1 << 12,
      max_operations: 16,
      max_requests: slots,
      ..ChannelAttrs::default()
    };
    asked.encode(&mut ops);
    asked.encode(&mut ops);
    match back {
      Some(program) => {
        ops.u32(program);
        ops.u32(1); // one flavor: AUTH_SYS with these parameters
        ops.u32(1);
        ops.fixed(&callback_authsys());
      }
      None => {
        ops.u32(0);
        ops.u32(0);
      }
    }
    let (status, results) = client.compound(1, ops.as_slice());
    assert_eq!(status, NFS4_OK, "CREATE_SESSION");
    client.sessionid.copy_from_slice(&results[8..8 + 16]);
    let flags = u32::from_be_bytes(results[8 + 16 + 4..8 + 16 + 8].try_into().unwrap());
    client.back_granted = flags & CONN_BACK_CHAN != 0;
    // CREATE_SESSION4resok after the session id: the sequence id and the flags, then the fore channel.
    let mut reader = XdrReader::new(&results[8 + 16 + 8..]);
    client.slots = ChannelAttrs::decode(&mut reader).unwrap().max_requests;
    client
  }

  /// SEQUENCE alone on slot `slot` (its first use, sequence id 1): the compound's status.
  fn sequence_on(&mut self, slot: u32) -> u32 {
    use slates_bridge_nfs::xdr::XdrWriter;
    let mut all = XdrWriter::new();
    all.u32(OP_SEQUENCE);
    all.fixed(&self.sessionid);
    all.u32(1);
    all.u32(slot);
    all.u32(slot);
    all.bool(false);
    self.compound(1, all.as_slice()).0
  }

  /// SEQUENCE on slot 0 with the next sequence id, then `ops` (`count` of them): the status and the
  /// results after the SEQUENCE result.
  fn sequenced(&mut self, count: u32, ops: &[u8]) -> (u32, Vec<u8>) {
    use slates_bridge_nfs::xdr::XdrWriter;
    /// Format: a SEQUENCE result's opnum and status words, then its body (session id, five words).
    const SEQUENCE_RESULT_BYTES: usize = 8 + 16 + 5 * 4;
    self.sequence += 1;
    let mut all = XdrWriter::new();
    all.u32(OP_SEQUENCE);
    all.fixed(&self.sessionid);
    all.u32(self.sequence);
    all.u32(0);
    all.u32(0);
    all.bool(true);
    all.fixed(ops);
    let (status, results) = self.compound(count + 1, all.as_slice());
    (
      status,
      results
        .get(SEQUENCE_RESULT_BYTES..)
        .unwrap_or_default()
        .to_vec(),
    )
  }
}

/// §4.6 A-35: an NFSv4.2 client reaches a volume on another shard through the daemon's one listener:
/// a LOOKUP of `<name>@<capability>` at the pseudo root enters the volume, OPEN creates a file, WRITE,
/// READ and CLOSE run under the open's state id, and an NFSv3 mount of the same capability reads the
/// same bytes back — both versions over one semantic layer, routed to the owner shard alike. Without
/// the capability the name is no entry, and a version other than 3 or 4 is `PROG_MISMATCH` naming 3–4.
#[test]
fn an_nfsv4_client_reaches_a_volume_on_another_shard_through_its_capability() {
  use slates_bridge_nfs::v4::types::{Bitmap, Stateid};
  use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
  let (daemon, instance) = two_shard_daemon("nfsv4");
  let mut client = Client::connect(&instance);
  let name = remote_volume_name(&mut client);
  let path = capability_path(&daemon, &name);
  let component = path.trim_start_matches('/').to_owned();
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut v4 = V4Client::connect(port);

  let (accept, range) = v4.rpc(2, 0, &[]);
  assert_eq!(accept, 2, "PROG_MISMATCH");
  assert_eq!(
    range[..8],
    [0, 0, 0, 3, 0, 0, 0, 4],
    "the versions served: 3 to 4"
  );

  let mut ops = XdrWriter::new();
  ops.u32(OP_PUTROOTFH);
  ops.u32(OP_LOOKUP);
  ops.opaque(name.as_bytes());
  let (status, _) = v4.sequenced(2, ops.as_slice());
  assert_eq!(
    status, NFS4ERR_NOENT,
    "a name without its capability is no entry"
  );

  let payload = b"written over NFSv4.2 into a volume on another shard\n";
  let mut ops = XdrWriter::new();
  ops.u32(OP_PUTROOTFH);
  ops.u32(OP_LOOKUP);
  ops.opaque(component.as_bytes());
  ops.u32(OP_OPEN);
  ops.u32(0); // seqid
  ops.u32(3); // share access both
  ops.u32(0); // deny none
  ops.u64(0);
  ops.opaque(b"owner-1");
  ops.u32(1); // OPEN4_CREATE
  ops.u32(1); // GUARDED4
  Bitmap::of(&[33]).encode(&mut ops); // FATTR4_MODE
  ops.opaque(&0o644u32.to_be_bytes());
  ops.u32(0); // CLAIM_NULL
  ops.opaque(b"v4.txt");
  ops.u32(OP_GETFH);
  let (status, results) = v4.sequenced(4, ops.as_slice());
  assert_eq!(status, NFS4_OK, "LOOKUP of the capability, then OPEN");
  let mut reader = XdrReader::new(&results);
  reader.fixed(8 + 8).unwrap(); // PUTROOTFH and LOOKUP results
  reader.fixed(8).unwrap(); // OPEN's opnum and status
  let stateid = Stateid::decode(&mut reader).unwrap();
  reader.fixed(4 + 8 + 8 + 4).unwrap();
  Bitmap::decode(&mut reader).unwrap();
  reader.u32().unwrap();
  reader.fixed(8).unwrap(); // GETFH's opnum and status
  let fh = reader.opaque(128).unwrap().to_vec();

  let mut ops = XdrWriter::new();
  ops.u32(OP_PUTFH);
  ops.opaque(&fh);
  ops.u32(OP_WRITE);
  stateid.encode(&mut ops);
  ops.u64(0);
  ops.u32(2); // FILE_SYNC4
  ops.opaque(payload);
  let (status, _) = v4.sequenced(2, ops.as_slice());
  assert_eq!(status, NFS4_OK, "WRITE");

  let mut ops = XdrWriter::new();
  ops.u32(OP_PUTFH);
  ops.opaque(&fh);
  ops.u32(OP_READ);
  stateid.encode(&mut ops);
  ops.u64(0);
  ops.u32(4096);
  let (status, results) = v4.sequenced(2, ops.as_slice());
  assert_eq!(status, NFS4_OK, "READ");
  let mut reader = XdrReader::new(&results);
  reader.fixed(8 + 8).unwrap();
  reader.u32().unwrap(); // eof
  assert_eq!(reader.opaque(4096).unwrap(), payload);

  let mut ops = XdrWriter::new();
  ops.u32(OP_PUTFH);
  ops.opaque(&fh);
  ops.u32(OP_CLOSE);
  ops.u32(0);
  stateid.encode(&mut ops);
  assert_eq!(v4.sequenced(2, ops.as_slice()).0, NFS4_OK, "CLOSE");

  let mut v3 = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = mount(&mut v3, &path, 1);
  let file = lookup(&mut v3, &root, "v4.txt", 2);
  assert_eq!(
    read(&mut v3, &file, 3),
    payload,
    "NFSv3 reads what NFSv4 wrote"
  );

  drop(v3);
  drop(v4);
  drop(client);
  drop(daemon);
}

/// The friendly name of a volume provisioned on a shard other than the NFS listener's (partition 0).
fn remote_volume_name(client: &mut Client) -> String {
  /// Shape: provisioning attempts before a volume lands off the control shard (two shards: each lands
  /// there with probability one half, so 32 misses are a 2^-32 event).
  const ATTEMPTS: usize = 32;
  let control_partition = 0;
  for attempt in 0..ATTEMPTS {
    let name = format!("v4vol-{attempt}");
    if let ReplyBody::Created { id } = client.call(&scratch(&name))
      && slates_server::verbs::owner_of(id) != control_partition
    {
      return name;
    }
  }
  panic!("no volume provisioned on a non-control shard");
}

/// A scratch volume provisioned on the control shard, the listener's (the mirror of [`remote_volume_name`]).
fn local_volume_name(client: &mut Client) -> String {
  /// Shape: see [`remote_volume_name`]'s attempts.
  const ATTEMPTS: usize = 32;
  for attempt in 0..ATTEMPTS {
    let name = format!("v4local-{attempt}");
    if let ReplyBody::Created { id } = client.call(&scratch(&name))
      && slates_server::verbs::owner_of(id) == 0
    {
      return name;
    }
  }
  panic!("no volume provisioned on the control shard");
}

/// Format: `OP_LOCK`, `OP_TEST_STATEID`, and `NFS4ERR_DENIED` / `NFS4ERR_LOCKS_HELD` /
/// `NFS4ERR_BAD_STATEID` (RFC 7863).
const OP_LOCK: u32 = 12;
const OP_TEST_STATEID: u32 = 55;
const NFS4ERR_BAD_STATEID: u32 = 10025;
/// Format: `CREATE_SESSION4_FLAG_CONN_BACK_CHAN` (RFC 8881 §18.36.1).
const CONN_BACK_CHAN: u32 = 2;
/// Format: `NFS4ERR_SHARE_DENIED` (RFC 8881 §15.1.8.4).
const NFS4ERR_SHARE_DENIED: u32 = 10015;
/// Format: `NFS4ERR_BADSLOT` (RFC 8881 §15.1.11.3).
const NFS4ERR_BADSLOT: u32 = 10053;
const NFS4ERR_DENIED: u32 = 10010;
const NFS4ERR_LOCKS_HELD: u32 = 10037;

impl V4Client {
  /// PUTROOTFH, LOOKUP of the capability component, OPEN (creating, unchecked) of `name` for this
  /// client's open-owner, GETFH: the open's state id and the file handle.
  fn open(
    &mut self,
    component: &str,
    name: &str,
  ) -> (slates_bridge_nfs::v4::types::Stateid, Vec<u8>) {
    let (open, fh, _) = self.open_creating(component, name);
    (open, fh)
  }

  /// OPEN of `name` under `component`, created if missing, for reading and writing: the open's state id, the file's
  /// handle, and the delegation granted with its kind (1 read, 2 write), if any.
  fn open_creating(
    &mut self,
    component: &str,
    name: &str,
  ) -> (
    slates_bridge_nfs::v4::types::Stateid,
    Vec<u8>,
    Option<(u32, slates_bridge_nfs::v4::types::Stateid)>,
  ) {
    use slates_bridge_nfs::v4::types::{Bitmap, Stateid};
    use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
    let mut ops = XdrWriter::new();
    ops.u32(OP_PUTROOTFH);
    ops.u32(OP_LOOKUP);
    ops.opaque(component.as_bytes());
    ops.u32(OP_OPEN);
    ops.u32(0);
    ops.u32(3);
    ops.u32(0);
    ops.u64(0);
    ops.opaque(b"open-owner");
    ops.u32(1); // OPEN4_CREATE
    ops.u32(0); // UNCHECKED4
    Bitmap::of(&[]).encode(&mut ops);
    ops.opaque(&[]);
    ops.u32(0); // CLAIM_NULL
    ops.opaque(name.as_bytes());
    ops.u32(OP_GETFH);
    let (status, results) = self.sequenced(4, ops.as_slice());
    assert_eq!(status, NFS4_OK, "OPEN");
    let mut reader = XdrReader::new(&results);
    reader.fixed(8 + 8 + 8).unwrap();
    let stateid = Stateid::decode(&mut reader).unwrap();
    reader.fixed(4 + 8 + 8 + 4).unwrap();
    Bitmap::decode(&mut reader).unwrap();
    let delegation = read_open_delegation(&mut reader);
    reader.fixed(8).unwrap();
    (stateid, reader.opaque(128).unwrap().to_vec(), delegation)
  }

  /// OPEN of an existing `name` under `component` by a new open-owner, asking for read access and denying writes:
  /// the compound's status.
  fn open_denying_writes(&mut self, component: &str, name: &str) -> u32 {
    self.open_with(component, name, 1, 2)
  }

  /// OPEN of an existing `name` under `component` by a new open-owner with share `access` and `deny`: the compound's
  /// status.
  fn open_with(&mut self, component: &str, name: &str, access: u32, deny: u32) -> u32 {
    use slates_bridge_nfs::xdr::XdrWriter;
    let mut ops = XdrWriter::new();
    ops.u32(OP_PUTROOTFH);
    ops.u32(OP_LOOKUP);
    ops.opaque(component.as_bytes());
    ops.u32(OP_OPEN);
    ops.u32(0);
    ops.u32(access);
    ops.u32(deny);
    ops.u64(0);
    ops.opaque(b"denying-owner");
    ops.u32(0); // OPEN4_NOCREATE
    ops.u32(0); // CLAIM_NULL
    ops.opaque(name.as_bytes());
    self.sequenced(3, ops.as_slice()).0
  }

  /// TEST_STATEID of one state id: its status in the reply.
  fn test_stateid(&mut self, stateid: slates_bridge_nfs::v4::types::Stateid) -> u32 {
    use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
    let mut ops = XdrWriter::new();
    ops.u32(OP_TEST_STATEID);
    ops.u32(1);
    stateid.encode(&mut ops);
    let (status, results) = self.sequenced(1, ops.as_slice());
    assert_eq!(status, NFS4_OK, "TEST_STATEID itself");
    let mut reader = XdrReader::new(&results);
    reader.fixed(8).unwrap();
    assert_eq!(reader.u32().unwrap(), 1);
    reader.u32().unwrap()
  }

  /// PUTFH `fh`, LOCK a write lock on `offset..offset+length` for the new lock-owner `owner` under
  /// the open `open`: the status and the lock operation's body.
  fn write_lock(
    &mut self,
    fh: &[u8],
    open: slates_bridge_nfs::v4::types::Stateid,
    (offset, length): (u64, u64),
    owner: &[u8],
  ) -> (u32, Vec<u8>) {
    use slates_bridge_nfs::xdr::XdrWriter;
    let mut ops = XdrWriter::new();
    ops.u32(OP_PUTFH);
    ops.opaque(fh);
    ops.u32(OP_LOCK);
    ops.u32(2); // WRITE_LT
    ops.bool(false);
    ops.u64(offset);
    ops.u64(length);
    ops.bool(true);
    ops.u32(0);
    open.encode(&mut ops);
    ops.u32(0);
    ops.u64(0);
    ops.opaque(owner);
    let (status, results) = self.sequenced(2, ops.as_slice());
    (status, results.get(8 + 8..).unwrap_or_default().to_vec())
  }
}

/// §4.6 A-36: a file's opens and locks live at its owner, so two NFSv4 clients reach the same lock
/// table whichever shard their listener is on. Over a volume on another shard, client A's write lock is
/// granted; client B's overlapping lock is `NFS4ERR_DENIED`, naming A's client and range; A's open
/// cannot close while its lock is held (`NFS4ERR_LOCKS_HELD`); and TEST_STATEID, which names no file,
/// reaches the owner through the state id itself.
#[test]
fn two_nfsv4_clients_meet_one_lock_table_at_the_files_owner() {
  use slates_bridge_nfs::xdr::XdrReader;
  let (daemon, instance) = two_shard_daemon("nfsv4locks");
  let mut client = Client::connect(&instance);
  let name = remote_volume_name(&mut client);
  let component = capability_path(&daemon, &name)
    .trim_start_matches('/')
    .to_owned();
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut a = V4Client::connect_as(port, b"host-a");
  let mut b = V4Client::connect_as(port, b"host-b");
  let (open_a, fh) = a.open(&component, "shared.db");
  let (open_b, fh_b) = b.open(&component, "shared.db");
  assert_eq!(fh, fh_b, "both opened the same file");

  let (status, _) = a.write_lock(&fh, open_a, (0, 10), b"a-locker");
  assert_eq!(status, NFS4_OK, "A's lock is granted");
  let (status, denied) = b.write_lock(&fh, open_b, (5, 1), b"b-locker");
  assert_eq!(status, NFS4ERR_DENIED, "B meets A's lock at the owner");
  let mut denied = XdrReader::new(&denied);
  assert_eq!((denied.u64().unwrap(), denied.u64().unwrap()), (0, 10));
  denied.u32().unwrap();
  assert_eq!(denied.u64().unwrap(), a.clientid, "the holder is A");

  let mut ops = slates_bridge_nfs::xdr::XdrWriter::new();
  ops.u32(OP_PUTFH);
  ops.opaque(&fh);
  ops.u32(OP_CLOSE);
  ops.u32(0);
  open_a.encode(&mut ops);
  assert_eq!(a.sequenced(2, ops.as_slice()).0, NFS4ERR_LOCKS_HELD);

  // TEST_STATEID names no file: the state id routes itself to its owner shard (D-14), which answers
  // for the client that holds it and refuses any other.
  assert_eq!(a.test_stateid(open_a), NFS4_OK, "A's open, tested by A");
  assert_eq!(
    b.test_stateid(open_a),
    NFS4ERR_BAD_STATEID,
    "A's open is not B's"
  );

  drop((a, b, client));
  drop(daemon);
}

/// The fsid of the object at the current handle after `ops` (their count `count`), from a trailing
/// GETATTR of FSID: the major word.
fn v4_fsid_after(v4: &mut V4Client, count: u32, ops: &[u8]) -> u64 {
  use slates_bridge_nfs::v4::types::Bitmap;
  use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
  /// Format: `OP_GETATTR`, `FATTR4_FSID`.
  const OP_GETATTR: u32 = 9;
  const FSID: u32 = 8;
  let mut all = XdrWriter::new();
  all.fixed(ops);
  all.u32(OP_GETATTR);
  Bitmap::of(&[FSID]).encode(&mut all);
  let (status, results) = v4.sequenced(count + 1, all.as_slice());
  assert_eq!(status, NFS4_OK, "the path and its GETATTR");
  // Every op before the GETATTR here has an empty result body: its opnum and status only.
  let mut reader = XdrReader::new(&results);
  reader.fixed(8 * usize::try_from(count).unwrap()).unwrap();
  reader.fixed(8).unwrap();
  Bitmap::decode(&mut reader).unwrap();
  XdrReader::new(reader.opaque(64).unwrap()).u64().unwrap()
}

/// §4.6 A-38: an NFSv4 client entering the pseudo-root scoped to a capability (`@<capability>`, as
/// NFSv3's `/@<capability>`) lists the volume it authorizes. The volume is on another shard, so the
/// gathered listing names it without attributes, and the front end fills them by a LOOKUP that routes
/// to the owner rather than dropping the entry; the entry carries the volume's own fsid, not the
/// pseudo-root's, so a client sees the filesystem boundary.
#[test]
fn an_nfsv4_scoped_browse_lists_a_volume_on_another_shard_with_its_own_fsid() {
  use slates_bridge_nfs::v4::types::Bitmap;
  use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
  /// Format: `OP_READDIR`, `FATTR4_FSID`.
  const OP_READDIR: u32 = 26;
  const FSID: u32 = 8;
  /// Shape: the READDIR budgets, far above one entry.
  const DIRCOUNT: u32 = 4096;
  const MAXCOUNT: u32 = 16384;
  let (daemon, instance) = two_shard_daemon("nfsv4-browse");
  let mut client = Client::connect(&instance);
  let name = remote_volume_name(&mut client);
  let scoped = root_capability_path(&daemon, &name);
  let scoped = scoped.trim_start_matches('/').to_owned();
  let entered = capability_path(&daemon, &name)
    .trim_start_matches('/')
    .to_owned();
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut v4 = V4Client::connect(port);

  let path = |component: &str| {
    let mut ops = XdrWriter::new();
    ops.u32(OP_PUTROOTFH);
    ops.u32(OP_LOOKUP);
    ops.opaque(component.as_bytes());
    ops.into_bytes()
  };
  let root_fsid = v4_fsid_after(&mut v4, 2, &path(&scoped));
  let volume_fsid = v4_fsid_after(&mut v4, 2, &path(&entered));
  assert_ne!(root_fsid, volume_fsid, "the volume is its own filesystem");

  let mut ops = XdrWriter::new();
  ops.fixed(&path(&scoped));
  ops.u32(OP_READDIR);
  ops.u64(0);
  ops.fixed(&[0; 8]);
  ops.u32(DIRCOUNT);
  ops.u32(MAXCOUNT);
  Bitmap::of(&[FSID]).encode(&mut ops);
  let (status, results) = v4.sequenced(3, ops.as_slice());
  assert_eq!(status, NFS4_OK, "READDIR of the scoped pseudo-root");
  let mut reader = XdrReader::new(&results);
  reader.fixed(8 + 8 + 8).unwrap(); // PUTROOTFH, LOOKUP, READDIR's opnum and status
  reader.fixed(8).unwrap(); // cookie verifier
  let mut listed = Vec::new();
  while reader.bool().unwrap() {
    reader.u64().unwrap(); // cookie
    let entry = String::from_utf8(reader.opaque(256).unwrap().to_vec()).unwrap();
    Bitmap::decode(&mut reader).unwrap();
    let fsid = XdrReader::new(reader.opaque(64).unwrap()).u64().unwrap();
    listed.push((entry, fsid));
  }
  assert_eq!(
    listed,
    vec![(name, volume_fsid)],
    "the authorized volume, on another shard, with its own fsid"
  );
  drop(v4);
  drop(client);
  drop(daemon);
}

/// Shape: the slots a client asks for, as Linux's `nfs.max_session_slots` default does (from memory).
const ASKED_SLOTS: u32 = 64;

/// §4.6 (A-75): do create a session asking for [`ASKED_SLOTS`] slots; expect the grant to be the daemon's bound on
/// one client's requests in flight (Little's law, the bound a ring client gets, `slots_per_ring`), not the shard
/// count, and the slots to be usable up to the grant and refused past it. Four slots (the shard count) held a
/// parallel Go build's requests in the Linux client: 1.05 ms queued per reopen against 0.44 ms on the wire
/// (2026-10-04).
#[test]
fn a_session_is_granted_one_clients_in_flight_bound_and_can_use_it() {
  let (daemon, instance) = two_shard_daemon("slots");
  let profile = common::machine_profile();
  let derived = DaemonConfig::derive(&profile, &instance, Some(2));
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut v4 = V4Client::connect_asking(port, b"slots-host", ASKED_SLOTS);
  let bound = derived.region.slots.min(ASKED_SLOTS);
  assert_eq!(v4.slots, bound, "the grant is one client's in-flight bound");
  assert!(v4.slots > 2, "more slots than shards: {}", v4.slots);
  assert_eq!(
    v4.sequence_on(v4.slots - 1),
    NFS4_OK,
    "the last granted slot serves"
  );
  assert_eq!(
    v4.sequence_on(v4.slots),
    NFS4ERR_BADSLOT,
    "a slot past the grant is refused"
  );
  drop(daemon);
}

/// The file handle of the volume root `component` names, over `v4`'s session.
fn volume_root(v4: &mut V4Client, component: &str) -> Vec<u8> {
  use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
  let mut ops = XdrWriter::new();
  ops.u32(OP_PUTROOTFH);
  ops.u32(OP_LOOKUP);
  ops.opaque(component.as_bytes());
  ops.u32(OP_GETFH);
  let (status, results) = v4.sequenced(3, ops.as_slice());
  assert_eq!(status, NFS4_OK, "the volume's root");
  let mut reader = XdrReader::new(&results);
  reader.fixed(8 + 8 + 8).unwrap();
  reader.opaque(128).unwrap().to_vec()
}

/// Sends `ops` on `v4`'s session from a new connection to `port`, expecting it served; the session's next sequence.
fn reach_from_a_second_connection(v4: &V4Client, port: u16, ops: &[u8]) -> u32 {
  let mut second = V4Client {
    stream: TcpStream::connect(("127.0.0.1", port)).unwrap(),
    clientid: v4.clientid,
    sessionid: v4.sessionid,
    sequence: v4.sequence,
    xid: 1000,
    slots: v4.slots,
    back_granted: false,
    server_owner: v4.server_owner.clone(),
  };
  let (status, _) = second.sequenced(2, ops);
  assert_eq!(
    status, NFS4_OK,
    "a second connection reaches the moved session"
  );
  second.sequence
}

/// Shape: compounds a moved session runs after its move, each making one v3 call.
const AFTER_MOVE: u32 = 20;

/// The refusal-map counter `name` summed over every shard.
fn counter(daemon: &Daemon, name: &str) -> u64 {
  daemon
    .refusals_on_every_shard()
    .unwrap()
    .get(name)
    .copied()
    .unwrap_or(0)
}

/// §4.6 (A-76): do reach a volume another shard owns over a session created at the connection's home, then run
/// [`AFTER_MOVE`] compounds; expect the session to have moved once with the connection to the volume's shard, so the
/// later compounds' v3 calls run there with no forward. Retry the last compound; expect its reply byte for byte from
/// the slot cache that moved. Name the session from a second connection; expect it served (home routes it on).
/// Destroy the session and create another; expect both served (the creation at home). With the session left at
/// home, a parallel Go build on a volume of another shard took 8.0 s against 4.3–4.7 s (2026-10-04).
#[test]
fn a_session_moves_to_its_volumes_shard_and_its_compounds_run_there() {
  use slates_bridge_nfs::xdr::XdrWriter;
  let (daemon, instance) = two_shard_daemon("v4move");
  let mut client = Client::connect(&instance);
  let name = remote_volume_name(&mut client);
  let path = capability_path(&daemon, &name);
  let component = path.trim_start_matches('/').to_owned();
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut v4 = V4Client::connect(port);
  let root = volume_root(&mut v4, &component);
  let forwarded_before = counter(&daemon, "nfs4.v3.forwarded");
  let local_before = counter(&daemon, "nfs4.v3.local");
  let mut access = XdrWriter::new();
  access.u32(OP_PUTFH);
  access.opaque(&root);
  access.u32(OP_ACCESS);
  access.u32(0x1f);
  let mut last = Vec::new();
  for _ in 0..AFTER_MOVE {
    let (status, results) = v4.sequenced(2, access.as_slice());
    assert_eq!(status, NFS4_OK, "ACCESS on the moved session");
    last = results;
  }
  assert_eq!(
    counter(&daemon, "nfs4.sessions.moved"),
    1,
    "the session moved once"
  );
  assert_eq!(
    counter(&daemon, "nfs4.v3.forwarded"),
    forwarded_before,
    "no compound after the move forwarded a call"
  );
  assert!(
    counter(&daemon, "nfs4.v3.local") >= local_before + u64::from(AFTER_MOVE),
    "the calls ran where the volume lives"
  );
  v4.sequence -= 1;
  let (status, replayed) = v4.sequenced(2, access.as_slice());
  assert_eq!(
    (status, &replayed),
    (NFS4_OK, &last),
    "the retry is the kept reply"
  );
  v4.sequence = reach_from_a_second_connection(&v4, port, access.as_slice());
  let mut destroy = XdrWriter::new();
  destroy.u32(OP_DESTROY_SESSION);
  destroy.fixed(&v4.sessionid);
  assert_eq!(
    v4.compound(1, destroy.as_slice()).0,
    NFS4_OK,
    "DESTROY_SESSION"
  );
  let again = V4Client::connect_as(port, b"v4-test-host");
  assert_ne!(again.sessionid, v4.sessionid, "a new session at home");
  drop(daemon);
}

/// Format: the callback program number the test client serves (any; the client names it at `CREATE_SESSION`).
const CB_PROGRAM: u32 = 0x4000_0123;
/// Format: `OP_CB_SEQUENCE` (RFC 8881 §20.9).
const OP_CB_SEQUENCE: u32 = 11;
/// Format: `OP_CB_RECALL` (RFC 8881 §20.2).
const OP_CB_RECALL: u32 = 4;
/// Shape: how long the test waits for the daemon to record a probe's outcome: the probe's deadline (one liveness
/// budget) and as much again for a loaded machine.
const PROBE_SETTLE: Duration = Duration::from_secs(4);

impl V4Client {
  /// Sends a `SEQUENCE` compound on slot 0 without waiting for its reply; its xid.
  fn send_sequence(&mut self) -> u32 {
    use slates_bridge_nfs::xdr::XdrWriter;
    use std::io::Write;
    self.sequence += 1;
    self.xid += 1;
    let mut seq = XdrWriter::new();
    seq.u32(OP_SEQUENCE);
    seq.fixed(&self.sessionid);
    seq.u32(self.sequence);
    seq.u32(0);
    seq.u32(0);
    seq.bool(false);
    let mut args = XdrWriter::new();
    args.opaque(b"");
    args.u32(2);
    args.u32(1);
    args.fixed(seq.as_slice());
    let mut body = XdrWriter::new();
    for field in [self.xid, 0, 2, 100_003, 4, 1, 0, 0, 0, 0] {
      body.u32(field);
    }
    body.fixed(args.as_slice());
    let marker = 0x8000_0000u32 | u32::try_from(body.len()).unwrap();
    self.stream.write_all(&marker.to_be_bytes()).unwrap();
    self.stream.write_all(body.as_slice()).unwrap();
    self.xid
  }

  /// The next record on the connection, whatever it carries: a reply to this client or a callback call to it.
  fn next_message(&mut self) -> Vec<u8> {
    use std::io::Read;
    // A message that never comes fails the test at the settle bound rather than hanging it.
    self.stream.set_read_timeout(Some(PROBE_SETTLE)).unwrap();
    let mut marker = [0u8; 4];
    self.stream.read_exact(&mut marker).unwrap();
    let mut body = vec![0u8; (u32::from_be_bytes(marker) & 0x7fff_ffff) as usize];
    self.stream.read_exact(&mut body).unwrap();
    body
  }

  /// Answers the callback `call` (a CB_COMPOUND whose first operation is CB_SEQUENCE) with `NFS4_OK`, after checking
  /// it names this client's program and session.
  fn answer_callback(&mut self, call: &[u8]) -> Option<slates_bridge_nfs::v4::types::Stateid> {
    self.answer_callback_with(call, NFS4_OK).0
  }

  /// Answers the callback `call` as [`V4Client::answer_callback`] does, with `status`: anything but `NFS4_OK` fails
  /// its `CB_SEQUENCE` (as the Linux client answers `NFS4ERR_DELAY`, the compound's status and the one result alike).
  /// The recalled state id and the call's sequence id.
  fn answer_callback_with(
    &mut self,
    call: &[u8],
    status: u32,
  ) -> (Option<slates_bridge_nfs::v4::types::Stateid>, u32) {
    use slates_bridge_nfs::v4::types::Stateid;
    use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
    use std::io::Write;
    let mut reader = XdrReader::new(call);
    let xid = check_callback_rpc(&mut reader);
    let (operations, sequenceid) = read_cb_sequence(&mut reader, &self.sessionid);
    let mut recalled = None;
    let mut results = XdrWriter::new();
    results.u32(OP_CB_SEQUENCE);
    results.u32(status);
    let answered = if status == NFS4_OK {
      results.fixed(&self.sessionid);
      for field in [sequenceid, 0, 0, 0] {
        results.u32(field);
      }
      operations
    } else {
      1
    };
    for _ in 1..answered {
      let opnum = reader.u32().unwrap();
      assert_eq!(
        opnum, OP_CB_RECALL,
        "the only other callback this server sends"
      );
      recalled = Some(Stateid::decode(&mut reader).unwrap());
      reader.bool().unwrap(); // truncate
      reader.opaque(128).unwrap(); // the file
      results.u32(OP_CB_RECALL);
      results.u32(0);
    }
    let mut body = XdrWriter::new();
    for field in [xid, 1, 0, 0, 0, 0] {
      body.u32(field);
    }
    body.u32(status); // CB_COMPOUND status
    body.opaque(b"");
    body.u32(answered);
    body.fixed(results.as_slice());
    let marker = 0x8000_0000u32 | u32::try_from(body.len()).unwrap();
    self.stream.write_all(&marker.to_be_bytes()).unwrap();
    self.stream.write_all(body.as_slice()).unwrap();
    (recalled, sequenceid)
  }
}

/// Reads a `CB_COMPOUND`'s header and its leading `CB_SEQUENCE`, checking the minor version, the operation and
/// `sessionid`: the compound's operation count and the call's sequence id, leaving `reader` at its next operation.
fn read_cb_sequence(
  reader: &mut slates_bridge_nfs::xdr::XdrReader<'_>,
  sessionid: &[u8],
) -> (u32, u32) {
  reader.opaque(1024).unwrap(); // tag
  assert_eq!(
    reader.u32().unwrap(),
    2,
    "the callback names the minor version the client's compounds use"
  );
  reader.u32().unwrap(); // callback_ident
  let operations = reader.u32().unwrap();
  assert!(operations >= 1);
  assert_eq!(reader.u32().unwrap(), OP_CB_SEQUENCE, "CB_SEQUENCE first");
  assert_eq!(reader.fixed(16).unwrap(), sessionid, "this session");
  let sequenceid = reader.u32().unwrap();
  for _ in 0..3 {
    reader.u32().unwrap(); // slot, highest slot, cache this
  }
  reader.u32().unwrap(); // referring call lists (none)
  (operations, sequenceid)
}

/// Reads an OPEN result's `open_delegation4` (RFC 8881 §18.16.2): the delegation's kind (1 read, 2 write) and state
/// id, if one was granted. A write delegation's space limit must be zero bytes (A-80).
fn read_open_delegation(
  reader: &mut slates_bridge_nfs::xdr::XdrReader<'_>,
) -> Option<(u32, slates_bridge_nfs::v4::types::Stateid)> {
  use slates_bridge_nfs::v4::types::Stateid;
  let kind = reader.u32().unwrap();
  if kind == 0 {
    return None;
  }
  let stateid = Stateid::decode(reader).unwrap();
  reader.bool().unwrap(); // recall
  if kind == 2 {
    assert_eq!(reader.u32().unwrap(), 1, "NFS_LIMIT_SIZE");
    assert_eq!(
      reader.u64().unwrap(),
      0,
      "a zero-byte limit: flushed at close"
    );
  }
  reader.fixed(4 + 4 + 4).unwrap(); // ace type, flags, mask
  reader.opaque(64).unwrap(); // who
  Some((kind, stateid))
}

/// Checks a callback's RPC header (a call of `CB_COMPOUND` to the client's program under its AUTH_SYS parameters) and
/// returns its xid, leaving `reader` at the compound's arguments.
fn check_callback_rpc(reader: &mut slates_bridge_nfs::xdr::XdrReader<'_>) -> u32 {
  let xid = reader.u32().unwrap();
  assert_eq!(reader.u32().unwrap(), 0, "a call");
  assert_eq!(reader.u32().unwrap(), 2, "RPC version 2");
  assert_eq!(
    reader.u32().unwrap(),
    CB_PROGRAM,
    "the client's callback program"
  );
  assert_eq!(
    (reader.u32().unwrap(), reader.u32().unwrap()),
    (1, 1),
    "CB_COMPOUND, version 1"
  );
  assert_eq!(
    reader.u32().unwrap(),
    1,
    "the callback carries AUTH_SYS, the flavor the client offered"
  );
  assert_eq!(
    reader.opaque(400).unwrap(),
    &callback_authsys()[..],
    "with the client's own parameters"
  );
  reader.u32().unwrap();
  reader.opaque(400).unwrap();
  xid
}

/// The `authsys_parms` the test client offers for callbacks: stamp, machine name, uid, gid, no supplementary gids.
fn callback_authsys() -> Vec<u8> {
  use slates_bridge_nfs::xdr::XdrWriter;
  let mut parms = XdrWriter::new();
  parms.u32(7);
  parms.opaque(b"cb-host");
  parms.u32(501);
  parms.u32(20);
  parms.u32(0);
  parms.into_bytes()
}

/// Whether a record body is an RPC call (message type 0) rather than a reply.
fn is_call(body: &[u8]) -> bool {
  body.get(4..8) == Some(&[0, 0, 0, 0][..])
}

/// Polls `name` on every shard until it reaches `at_least` or [`PROBE_SETTLE`] passes; whether it did.
fn counter_reaches(daemon: &Daemon, name: &str, at_least: u64) -> bool {
  let started = Instant::now();
  while started.elapsed() < PROBE_SETTLE {
    if counter(daemon, name) >= at_least {
      return true;
    }
    std::thread::yield_now();
  }
  false
}

/// §4.6, RFC 8881 §2.10.3.1 and §10.2 (B-1): do create a session asking for the back channel on its connection,
/// then send a compound on it; expect the back channel granted, the daemon's probe (a `CB_COMPOUND` whose
/// `CB_SEQUENCE` names the session) arriving on the same connection interleaved with the compound's reply, and, once
/// answered `NFS4_OK`, the back channel recorded up. Then do the same with a client that never answers; expect the
/// back channel recorded down once the probe's deadline passes, and that client's compounds still answered.
#[test]
fn a_back_channel_is_probed_on_its_connection_and_recorded_up_or_down() {
  let (daemon, _instance) = two_shard_daemon("backchan");
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut answering = V4Client::connect_full(port, b"cb-answers", 4, Some(CB_PROGRAM));
  assert!(
    answering.back_granted,
    "the back channel is granted on the connection"
  );
  let xid = answering.send_sequence();
  let (mut replied, mut probed) = (false, false);
  while !(replied && probed) {
    let message = answering.next_message();
    if is_call(&message) {
      answering.answer_callback(&message);
      probed = true;
    } else {
      assert_eq!(
        message.get(..4),
        Some(&xid.to_be_bytes()[..]),
        "the compound's reply"
      );
      replied = true;
    }
  }
  assert!(
    counter_reaches(&daemon, "nfs4.callback.up", 1),
    "the answered probe marks the channel up"
  );
  let mut silent = V4Client::connect_full(port, b"cb-silent", 4, Some(CB_PROGRAM));
  let xid = silent.send_sequence();
  loop {
    let message = silent.next_message();
    if !is_call(&message) {
      assert_eq!(message.get(..4), Some(&xid.to_be_bytes()[..]));
      break;
    }
  }
  assert!(
    counter_reaches(&daemon, "nfs4.callback.down", 1),
    "an unanswered probe marks the channel down"
  );
  drop(daemon);
}

/// §4.6 A-36, RFC 8881 §9: a file's locks and share reservations are the file's, whichever mount reaches it. Do
/// open one file from two NFSv4 clients through two mounts of the volume (two attachments, so two capabilities and
/// two handles for the same file), lock a range from A, then lock an overlapping range from B; expect B denied by A's
/// lock. Then open the file through a third mount from a third client C (no open of its own), denying writes while A
/// and B hold it open for writing; expect `NFS4ERR_SHARE_DENIED`.
/// Keyed by the handle's bytes, which carry the capability, the two mounts' locks and shares never met (2026-10-04:
/// two Docker containers, each with its own `slates export`, could both hold a write lock on one range).
#[test]
fn locks_and_shares_meet_across_two_mounts_of_one_file() {
  let (daemon, instance) = two_shard_daemon("nfsv4mounts");
  let mut client = Client::connect(&instance);
  let name = remote_volume_name(&mut client);
  let first = capability_path(&daemon, &name)
    .trim_start_matches('/')
    .to_owned();
  let second = capability_path(&daemon, &name)
    .trim_start_matches('/')
    .to_owned();
  assert_ne!(first, second, "two mounts, two capabilities");
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut a = V4Client::connect_as(port, b"mount-a");
  let mut b = V4Client::connect_as(port, b"mount-b");
  let (open_a, fh_a) = a.open(&first, "shared.db");
  let (open_b, fh_b) = b.open(&second, "shared.db");
  assert_ne!(fh_a, fh_b, "the same file under two capabilities");
  let (status, _) = a.write_lock(&fh_a, open_a, (0, 10), b"a-locker");
  assert_eq!(status, NFS4_OK, "A's lock is granted");
  let (status, _) = b.write_lock(&fh_b, open_b, (5, 1), b"b-locker");
  assert_eq!(
    status, NFS4ERR_DENIED,
    "B meets A's lock through its own mount"
  );
  // C comes through a third mount and holds no open of its own, so only the others' opens of the file, through
  // other mounts, can refuse its share.
  let third = capability_path(&daemon, &name)
    .trim_start_matches('/')
    .to_owned();
  let mut c = V4Client::connect_as(port, b"mount-c");
  assert_eq!(
    c.open_denying_writes(&third, "shared.db"),
    NFS4ERR_SHARE_DENIED,
    "C cannot deny writes while A holds the file open for writing through another mount"
  );
  drop((a, b, c, client));
  drop(daemon);
}

/// Shape: the lease the recall test runs under, short so that the file it writes settles past the delegation quiet
/// period (the lease) within the test.
const RECALL_LEASE: Duration = Duration::from_millis(300);
/// Format: `OP_DELEGRETURN` (RFC 8881 §18.6).
const OP_DELEGRETURN: u32 = 8;
/// Format: `NFS4ERR_DELAY` (RFC 8881 §15.1.1.3).
const NFS4ERR_DELAY: u32 = 10008;

impl V4Client {
  /// Sends one compound and answers its SEQUENCE's probe while waiting, until the back channel is recorded up.
  fn prove_back_channel(&mut self, daemon: &Daemon) {
    let xid = self.send_sequence();
    let (mut replied, mut probed) = (false, false);
    while !(replied && probed) {
      let message = self.next_message();
      if is_call(&message) {
        self.answer_callback(&message);
        probed = true;
      } else {
        assert_eq!(message.get(..4), Some(&xid.to_be_bytes()[..]));
        replied = true;
      }
    }
    assert!(
      counter_reaches(daemon, "nfs4.callback.up", 1),
      "the back channel is up"
    );
  }

  /// OPEN of an existing `name` under `component` for reading, denying nothing: the open's state id, the file's handle,
  /// and the read delegation's state id when one was granted.
  fn open_for_reading(
    &mut self,
    component: &str,
    name: &str,
  ) -> (
    slates_bridge_nfs::v4::types::Stateid,
    Vec<u8>,
    Option<slates_bridge_nfs::v4::types::Stateid>,
  ) {
    use slates_bridge_nfs::v4::types::{Bitmap, Stateid};
    use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
    let mut ops = XdrWriter::new();
    ops.u32(OP_PUTROOTFH);
    ops.u32(OP_LOOKUP);
    ops.opaque(component.as_bytes());
    ops.u32(OP_OPEN);
    ops.u32(0);
    ops.u32(1); // share access READ
    ops.u32(0); // deny none
    ops.u64(0);
    ops.opaque(b"reading-owner");
    ops.u32(0); // OPEN4_NOCREATE
    ops.u32(0); // CLAIM_NULL
    ops.opaque(name.as_bytes());
    ops.u32(OP_GETFH);
    let (status, results) = self.sequenced(4, ops.as_slice());
    assert_eq!(status, NFS4_OK, "OPEN for reading");
    let mut reader = XdrReader::new(&results);
    reader.fixed(8 + 8 + 8).unwrap();
    let open = Stateid::decode(&mut reader).unwrap();
    reader.fixed(4 + 8 + 8 + 4).unwrap();
    Bitmap::decode(&mut reader).unwrap();
    let delegation = read_open_delegation(&mut reader).map(|(kind, stateid)| {
      assert_eq!(kind, 1, "a read-only open is read delegated");
      stateid
    });
    reader.fixed(8).unwrap();
    (open, reader.opaque(128).unwrap().to_vec(), delegation)
  }

  /// WRITE of `bytes` at offset 0 of `fh` under `stateid`, FILE_SYNC: the compound's status.
  fn write_under(
    &mut self,
    fh: &[u8],
    stateid: slates_bridge_nfs::v4::types::Stateid,
    bytes: &[u8],
  ) -> u32 {
    use slates_bridge_nfs::xdr::XdrWriter;
    let mut ops = XdrWriter::new();
    ops.u32(OP_PUTFH);
    ops.opaque(fh);
    ops.u32(OP_WRITE);
    stateid.encode(&mut ops);
    ops.u64(0);
    ops.u32(2); // FILE_SYNC4
    ops.opaque(bytes);
    self.sequenced(2, ops.as_slice()).0
  }

  /// GETATTR of `fh`'s size: the compound's status.
  fn getattr_status(&mut self, fh: &[u8]) -> u32 {
    use slates_bridge_nfs::v4::types::Bitmap;
    use slates_bridge_nfs::xdr::XdrWriter;
    /// Format: `OP_GETATTR`.
    const OP_GETATTR: u32 = 9;
    /// Format: `FATTR4_SIZE`.
    const FATTR4_SIZE: u32 = 4;
    let mut ops = XdrWriter::new();
    ops.u32(OP_PUTFH);
    ops.opaque(fh);
    ops.u32(OP_GETATTR);
    Bitmap::of(&[FATTR4_SIZE]).encode(&mut ops);
    self.sequenced(2, ops.as_slice()).0
  }

  /// CLOSE of `stateid` on `fh`: the compound's status.
  fn close(&mut self, fh: &[u8], stateid: slates_bridge_nfs::v4::types::Stateid) -> u32 {
    use slates_bridge_nfs::xdr::XdrWriter;
    let mut ops = XdrWriter::new();
    ops.u32(OP_PUTFH);
    ops.opaque(fh);
    ops.u32(OP_CLOSE);
    ops.u32(0);
    stateid.encode(&mut ops);
    self.sequenced(2, ops.as_slice()).0
  }

  /// DELEGRETURN of `delegation` on `fh`: the compound's status.
  fn delegreturn(&mut self, fh: &[u8], delegation: slates_bridge_nfs::v4::types::Stateid) -> u32 {
    use slates_bridge_nfs::xdr::XdrWriter;
    let mut ops = XdrWriter::new();
    ops.u32(OP_PUTFH);
    ops.opaque(fh);
    ops.u32(OP_DELEGRETURN);
    delegation.encode(&mut ops);
    self.sequenced(2, ops.as_slice()).0
  }
}

/// RFC 8881 §10.2, §10.4, §10.4.4 (A-78, A-79, A-80): do give client A an answering back channel and open a file from
/// it for reading; expect a read delegation. Open the file for writing from client B; expect `NFS4ERR_DELAY`, and A
/// to receive a `CB_RECALL` naming its delegation on its own connection. Answer it and return the delegation; expect
/// B's retried open and its write to succeed. The volume is on the listener's own shard here, where a session's files are owned,
/// so the grant is allowed; the daemon runs a short lease so the file settles past the quiet period quickly.
#[test]
fn a_write_from_another_client_recalls_the_read_delegation_first() {
  let (daemon, instance) = two_shard_daemon_with("recall", |config| {
    config.failover_slo_ns = RECALL_LEASE.as_nanos().try_into().unwrap();
  });
  let mut client = Client::connect(&instance);
  let name = local_volume_name(&mut client);
  let component = capability_path(&daemon, &name)
    .trim_start_matches('/')
    .to_owned();
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut b = V4Client::connect_as(port, b"writer-b");
  let (open_b, fh) = b.open(&component, "held.txt");
  assert_eq!(b.write_under(&fh, open_b, b"before"), NFS4_OK);
  assert_eq!(b.close(&fh, open_b), NFS4_OK);
  // A file changed within the lease is not delegated (A-78): the test waits the short lease out.
  let written = Instant::now();
  while written.elapsed() < RECALL_LEASE + RECALL_LEASE / 2 {
    std::thread::yield_now();
  }
  let mut a = V4Client::connect_full(port, b"holder-a", 4, Some(CB_PROGRAM));
  a.prove_back_channel(&daemon);
  let (_, fh_a, delegation) = a.open_for_reading(&component, "held.txt");
  let delegation = delegation.expect("A's read-only open is delegated");
  // B's open for writing conflicts with A's read delegation (RFC 8881 §10.4.4; A-80): it waits at the open.
  assert_eq!(
    b.open_with(&component, "held.txt", 3, 0),
    NFS4ERR_DELAY,
    "B waits for A's delegation"
  );
  let recall = a.next_message();
  assert!(is_call(&recall), "A is called back");
  assert_eq!(
    a.answer_callback(&recall),
    Some(delegation),
    "the recall names A's delegation"
  );
  assert_eq!(a.delegreturn(&fh_a, delegation), NFS4_OK, "A returns it");
  let (open_b, _) = b.open(&component, "held.txt");
  assert_eq!(
    b.write_under(&fh, open_b, b"after!"),
    NFS4_OK,
    "B's retry proceeds"
  );
  assert!(counter(&daemon, "nfs4.recall.sent") >= 1);
  drop((a, b, client));
  drop(daemon);
}

/// Delegates `held.txt` in a fresh short-lease daemon's volume to client A (answering back channel): the daemon, its
/// instance's client, A, A's handle and delegation, and an NFSv3 connection with the file's v3 handle.
fn delegated_to_a(
  name: &str,
) -> (
  Daemon,
  Client,
  V4Client,
  Vec<u8>,
  slates_bridge_nfs::v4::types::Stateid,
  TcpStream,
  Vec<u8>,
) {
  let (daemon, instance) = two_shard_daemon_with(name, |config| {
    config.failover_slo_ns = RECALL_LEASE.as_nanos().try_into().unwrap();
  });
  let mut client = Client::connect(&instance);
  let volume = local_volume_name(&mut client);
  let path = capability_path(&daemon, &volume);
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut v3 = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = mount(&mut v3, &path, 1);
  let file = create(&mut v3, &root, "held.txt", 2);
  write(&mut v3, &file, b"before", 3);
  // A file changed within the lease is not delegated (A-78): the test waits the short lease out.
  let written = Instant::now();
  while written.elapsed() < RECALL_LEASE + RECALL_LEASE / 2 {
    std::thread::yield_now();
  }
  let mut a = V4Client::connect_full(port, b"holder-a", 4, Some(CB_PROGRAM));
  a.prove_back_channel(&daemon);
  let component = path.trim_start_matches('/').to_owned();
  let (_, fh_a, delegation) = a.open_for_reading(&component, "held.txt");
  let delegation = delegation.expect("A's read-only open is delegated");
  (daemon, client, a, fh_a, delegation, v3, file)
}

/// Writes `data` to `file` over NFSv3 on another thread: its status and how long it took.
fn write_in_background(
  mut v3: TcpStream,
  file: Vec<u8>,
  data: &'static [u8],
) -> std::thread::JoinHandle<(u32, Duration)> {
  std::thread::spawn(move || {
    let began = Instant::now();
    let status = common::nfs::write_status(&mut v3, &file, data, 10);
    (status, began.elapsed())
  })
}

/// RFC 8881 §10.2 (A-79): do write a file A holds a read delegation of, over NFSv3; expect the write held, not
/// answered `NFS3ERR_JUKEBOX` (whose client retry backs off for seconds), A recalled, and the write to succeed once A
/// answers and returns the delegation, with nothing revoked (the return, not the lease, released it).
#[test]
fn an_nfsv3_write_to_a_delegated_file_is_held_until_the_delegation_returns() {
  let (daemon, client, mut a, fh_a, delegation, v3, file) = delegated_to_a("held-v3");
  let writer = write_in_background(v3, file, b"after!");
  let recall = a.next_message();
  assert!(is_call(&recall), "A is called back");
  assert_eq!(a.answer_callback(&recall), Some(delegation));
  assert_eq!(a.delegreturn(&fh_a, delegation), NFS4_OK, "A returns it");
  let (status, took) = writer.join().unwrap();
  assert_eq!(status, 0, "the held write succeeded (took {took:?})");
  assert!(
    counter(&daemon, "nfs.v3.held_for_recall") >= 1,
    "it was held"
  );
  assert_eq!(
    counter(&daemon, "nfs4.delegation.revoked_lapsed"),
    0,
    "released by the return"
  );
  drop((a, client));
  drop(daemon);
}

/// RFC 8881 §10.4.5 (A-79): do write a file A holds a read delegation of, over NFSv3, while A answers the recall but
/// never returns the delegation; expect the write held for one lease, the delegation then revoked, and the write to
/// succeed.
#[test]
fn an_nfsv3_write_held_on_an_unreturned_delegation_proceeds_after_its_revocation() {
  let (daemon, client, mut a, _fh_a, delegation, v3, file) = delegated_to_a("held-v3-lapse");
  let writer = write_in_background(v3, file, b"after!");
  let recall = a.next_message();
  assert_eq!(a.answer_callback(&recall), Some(delegation));
  let (status, took) = writer.join().unwrap();
  assert_eq!(status, 0, "the held write succeeded");
  assert!(took >= RECALL_LEASE, "held for the lease, took {took:?}");
  assert!(counter(&daemon, "nfs4.delegation.revoked_lapsed") >= 1);
  drop((a, client));
  drop(daemon);
}

/// RFC 8881 §15.1.1.3, §20.9.3: do answer the back channel's probe `NFS4ERR_DELAY` (as the Linux client does while it
/// is still setting its session up); expect the probe sent again with the same sequence id (a failed `CB_SEQUENCE`
/// leaves the slot unchanged), and the back channel up once that one is answered `NFS4_OK`.
#[test]
fn a_probe_answered_delay_is_retried_on_the_same_slot_sequence() {
  let (daemon, _instance) = two_shard_daemon("probe-delay");
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut a = V4Client::connect_full(port, b"delaying-a", 4, Some(CB_PROGRAM));
  let xid = a.send_sequence();
  let mut first: Option<u32> = None;
  let mut replied = false;
  let mut proven = false;
  while !(replied && proven) {
    let message = a.next_message();
    if !is_call(&message) {
      assert_eq!(message.get(..4), Some(&xid.to_be_bytes()[..]));
      replied = true;
      continue;
    }
    match first {
      None => first = Some(a.answer_callback_with(&message, NFS4ERR_DELAY).1),
      Some(sequenceid) => {
        let (_, again) = a.answer_callback_with(&message, NFS4_OK);
        assert_eq!(again, sequenceid, "the same slot sequence");
        proven = true;
      }
    }
  }
  assert!(
    counter_reaches(&daemon, "nfs4.callback.up", 1),
    "the back channel is up after the retry"
  );
  assert_eq!(
    counter(&daemon, "nfs4.callback.down"),
    0,
    "never marked down"
  );
  drop(a);
  drop(daemon);
}

/// A client with an answering back channel on a short-lease daemon, and a second client without one: the daemon,
/// its instance's client, the holder, the other client, and the volume's component.
fn holder_and_other(name: &str) -> (Daemon, Client, V4Client, V4Client, String) {
  let (daemon, instance) = two_shard_daemon_with(name, |config| {
    config.failover_slo_ns = RECALL_LEASE.as_nanos().try_into().unwrap();
  });
  let mut client = Client::connect(&instance);
  let volume = local_volume_name(&mut client);
  let component = capability_path(&daemon, &volume)
    .trim_start_matches('/')
    .to_owned();
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut holder = V4Client::connect_full(port, b"holder-a", 4, Some(CB_PROGRAM));
  holder.prove_back_channel(&daemon);
  let other = V4Client::connect_as(port, b"other-b");
  (daemon, client, holder, other, component)
}

/// RFC 8881 §10.4, §10.4.1, §10.4.4 (A-80): do create a file for writing from A, whose back channel answers; expect a
/// write delegation with a zero-byte space limit, and A's writes under its open and under the delegation to succeed
/// (a holder's own change passes the recall gate). Open the file from B for reading; expect `NFS4ERR_DELAY`, A
/// recalled on its own connection, and B's open to succeed once A returns the delegation.
#[test]
fn a_write_delegation_serves_its_holders_writes_and_any_other_open_recalls_it() {
  let (daemon, client, mut a, mut b, component) = holder_and_other("write-deleg");
  let (open_a, fh, delegation) = a.open_creating(&component, "w.txt");
  let (kind, delegation) = delegation.expect("A's open for writing is delegated");
  assert_eq!(kind, 2, "a write delegation");
  assert_eq!(
    a.write_under(&fh, open_a, b"mine"),
    NFS4_OK,
    "under the open"
  );
  assert_eq!(
    a.write_under(&fh, delegation, b"mine2"),
    NFS4_OK,
    "under the delegation"
  );
  assert_eq!(
    b.open_with(&component, "w.txt", 1, 0),
    NFS4ERR_DELAY,
    "B waits for A's write delegation"
  );
  let recall = a.next_message();
  assert!(is_call(&recall), "A is called back");
  assert_eq!(a.answer_callback(&recall), Some(delegation));
  assert_eq!(a.delegreturn(&fh, delegation), NFS4_OK);
  assert_eq!(b.open_with(&component, "w.txt", 1, 0), NFS4_OK, "B's retry");
  drop((a, b, client));
  drop(daemon);
}

/// RFC 8881 §10.4.3 (A-80): do read the attributes of a file A holds a write delegation of, from B; expect
/// `NFS4ERR_DELAY` and A recalled. A's own GETATTR is answered.
#[test]
fn another_clients_getattr_of_a_write_delegated_file_recalls_it() {
  let (daemon, client, mut a, mut b, component) = holder_and_other("write-getattr");
  let (_, fh, delegation) = a.open_creating(&component, "g.txt");
  let (_, delegation) = delegation.expect("delegated");
  assert_eq!(a.getattr_status(&fh), NFS4_OK, "the holder's own");
  assert_eq!(b.getattr_status(&fh), NFS4ERR_DELAY, "another client waits");
  let recall = a.next_message();
  assert_eq!(a.answer_callback(&recall), Some(delegation));
  assert_eq!(a.delegreturn(&fh, delegation), NFS4_OK);
  assert_eq!(b.getattr_status(&fh), NFS4_OK);
  assert!(counter(&daemon, "nfs4.delegation.getattr_recalled") >= 1);
  drop((a, b, client));
  drop(daemon);
}

/// RFC 8881 §10.4.4 (A-78, A-80): do open, from B, a file A holds a read delegation of, for reading while denying
/// reads; expect `NFS4ERR_DELAY` and A recalled (A serves its own read opens locally, which B's deny would refuse).
/// Before A-80 B's open was granted with A's delegation outstanding.
#[test]
fn an_open_denying_reads_recalls_a_read_delegation() {
  let (daemon, client, mut a, mut b, component) = holder_and_other("deny-read");
  let (open_a, fh, created) = a.open_creating(&component, "r.txt");
  let (_, created) = created.expect("the create is write delegated");
  assert_eq!(a.delegreturn(&fh, created), NFS4_OK);
  assert_eq!(a.close(&fh, open_a), NFS4_OK);
  // A file changed within the lease is not read delegated (A-78): the test waits the short lease out.
  let written = Instant::now();
  while written.elapsed() < RECALL_LEASE + RECALL_LEASE / 2 {
    std::thread::yield_now();
  }
  let (_, _, delegation) = a.open_for_reading(&component, "r.txt");
  let delegation = delegation.expect("A's read-only open is delegated");
  assert_eq!(b.open_with(&component, "r.txt", 1, 1), NFS4ERR_DELAY);
  let recall = a.next_message();
  assert_eq!(a.answer_callback(&recall), Some(delegation));
  drop((a, b, client));
  drop(daemon);
}

/// RFC 8881 §2.4, §2.10.5: do start two fresh daemons and exchange ids with each as the same client; expect different
/// server-owner major ids and different client ids. A client takes two servers that share both for one server and
/// shares one client between them (Linux `nfs41_walk_client_list`): before the fix every fresh daemon answered major
/// id 1 and client id 1:1, so a Docker host's kernel queued a new daemon's mount behind a dead one's
/// (docs/bugs/2026-10-04-every-daemon-announced-one-nfs-server-owner.md).
#[test]
fn two_fresh_daemons_name_different_servers_and_mint_different_client_ids() {
  let (first, _first_instance) = two_shard_daemon("owner-first");
  let (second, _second_instance) = two_shard_daemon("owner-second");
  let a = V4Client::connect_as(first.nfs_port().unwrap(), b"one-client");
  let b = V4Client::connect_as(second.nfs_port().unwrap(), b"one-client");
  assert_ne!(a.server_owner, b.server_owner, "two servers, two owners");
  assert_ne!(
    a.clientid, b.clientid,
    "client ids never repeat across servers' lives"
  );
  drop((a, b));
  drop((first, second));
}

/// §4.12 `ReadDir` on a plain volume: do write two files and a directory into a provisioned volume through its NFS
/// mount, and list its root and the directory over the typed channel; expect every entry, each file with the size
/// written and the directory as one, and a path under a file refused.
#[test]
fn a_plain_volume_lists_what_its_mount_wrote() {
  use slates_ipc::protocol::{DirEntry, EntryKind, ReadAt};
  let (daemon, instance) = single_shard_daemon("readdir");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id, .. } = client.call(&scratch("listed")) else {
    panic!("the volume was not created");
  };
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = mount(&mut stream, &capability_path(&daemon, "listed"), 1);
  let a = create(&mut stream, &root, "a.txt", 2);
  write(&mut stream, &a, b"hello", 3);
  let b = create(&mut stream, &root, "b.bin", 4);
  write(&mut stream, &b, &[7u8; 300], 5);
  let sub = common::nfs::mkdir(&mut stream, &root, "sub", 6);
  let c = create(&mut stream, &sub, "c", 7);
  write(&mut stream, &c, b"!", 8);
  let list = |client: &mut Client, path: &str| {
    client.call(&RequestBody::ReadDir {
      volume: id,
      path: path.to_owned(),
      at: ReadAt::Head,
      cursor: 0,
    })
  };
  let ReplyBody::DirPage { mut entries, next } = list(&mut client, "") else {
    panic!("a page");
  };
  entries.sort_by(|x, y| x.name.cmp(&y.name));
  assert_eq!(next, None, "one page holds three entries");
  let entry = |name: &str, kind, size| DirEntry {
    name: name.to_owned(),
    kind,
    size,
  };
  assert_eq!(
    entries,
    vec![
      entry("a.txt", EntryKind::File, 5),
      entry("b.bin", EntryKind::File, 300),
      entry("sub", EntryKind::Dir, 0),
    ]
  );
  let ReplyBody::DirPage { entries, .. } = list(&mut client, "/sub") else {
    panic!("a page");
  };
  assert_eq!(entries, vec![entry("c", EntryKind::File, 1)]);
  assert!(
    matches!(list(&mut client, "a.txt/x"), ReplyBody::Refused { .. }),
    "a path under a file is refused"
  );
  drop(stream);
  drop(client);
  drop(daemon);
}
