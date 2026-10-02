//! The daemon serves a guest device on a volume it provisioned (§4.6 A-9, D-2, RQ-20 "host
//! processes, OCI containers and Linux microVM guests can consume the same VFS"; AC-4.11/T-4.13's
//! guest leg, AC-4.12): a single-shard daemon is started, a client provisions a volume through the
//! real rendezvous, the harness attaches a guest device to it over an in-process seam whose
//! doorbell is a pipe, and a simulated guest driver on the volume's owning shard creates a file,
//! writes bytes and releases it through real virtqueues; then — over the daemon's own NFS loopback
//! port, the host mount path — a client mounts the same volume and reads the guest's bytes back.
//! One VFS, two transports, byte for byte. A second guest whose consumer is not on the volume's
//! access list is admitted (authenticated) but every effect is refused by the seam (§4.13).
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// The guest transport rides the runtime's Unix descriptor readiness.
#![cfg(unix)]

use std::net::TcpStream;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use slates_bridge_fuse::abi::{OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::reply::EntryOut;
use slates_bridge_virtiofs::admission::GuestTransport;
use slates_bridge_virtiofs::serve::EndReason;
use slates_db::catalog::Principal;
use slates_ipc::protocol::{
  Direction, NamePolicy, ReplyBody, RequestBody, SizeClass, pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_server::virtiofs::{GuestDeviceOutcome, guest_transport_capabilities};
use slates_server::{Daemon, DaemonConfig, SegmentSource};
use slates_wire::request::RequestId;

mod common;
use common::anchor::{anchor_segment, source_of};
use common::guest::{
  create_body, flush_body, message, my_uid, release_body, reply_error, round_trip, run_guest,
  run_guest_viewing, u64_at, write_body,
};
use common::nfs::{lookup, mount, read};

/// Shape: the reply deadline (nanoseconds): five seconds, far past any served verb.
const DEADLINE_NS: u64 = 5_000_000_000;
/// Shape: how long a client waits for the daemon or a full ring before giving up.
const CREDIT_WAIT: Duration = Duration::from_secs(5);
/// Format: `EPERM`.
const EPERM: i32 = 1;
/// The bytes the guest writes and the host reads back.
const PAYLOAD: &[u8] = b"written by a guest through virtqueues, read back by the host over NFS\n";

// --- The client side of the daemon's own rendezvous (as in tests/nfs_mount.rs). ---

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

/// RQ-20 by use: a guest creates, writes and releases a file through virtqueues on a
/// daemon-provisioned volume; the host reads the same bytes back over the daemon's NFS mount path;
/// the device loop ended through the terminal step when the guest hung up; the daemon's
/// capability report offers the in-process transport and never DAX.
#[test]
fn a_guest_writes_a_file_into_a_daemon_volume_that_the_host_reads_back_over_nfs() {
  let (daemon, instance) = single_shard_daemon("virtiofs");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("guest")) else {
    panic!("the volume was not created");
  };

  let (outcome, errors) = run_guest(
    &daemon,
    id,
    Principal::Uid { uid: my_uid() },
    |kick_write, call_read| {
      Box::pin(async move {
        let created = round_trip(
          &kick_write,
          &call_read,
          &message(
            Opcode::Create.to_wire(),
            1,
            1,
            &create_body("from-guest.txt"),
          ),
        )
        .await;
        let ino = u64_at(&created, OUT_HEADER_LEN);
        let fh = u64_at(&created, OUT_HEADER_LEN + EntryOut::LEN);
        let written = round_trip(
          &kick_write,
          &call_read,
          &message(Opcode::Write.to_wire(), 2, ino, &write_body(fh, PAYLOAD)),
        )
        .await;
        let released = round_trip(
          &kick_write,
          &call_read,
          &message(Opcode::Release.to_wire(), 3, ino, &release_body(fh)),
        )
        .await;
        drop(kick_write);
        (
          reply_error(&created),
          reply_error(&written),
          reply_error(&released),
        )
      })
    },
  );
  assert_eq!(
    errors,
    (0, 0, 0),
    "create, write and release succeeded in the guest"
  );
  let GuestDeviceOutcome::Ended(end) = outcome else {
    panic!("the device did not serve: {outcome:?}");
  };
  assert_eq!(end.why, EndReason::DoorbellHungUp);
  assert!(
    end.passes >= 3,
    "one pass per request at least: {}",
    end.passes
  );
  assert!(
    end
      .reclaimed
      .expect("the terminal step ran")
      .references_swept
  );

  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  // Every mount presents a capability (AUD-01); the daemon mints the owner's for the test.
  let capability = daemon
    .mount_capability("guest")
    .expect("the owner shard answers")
    .expect("the guest volume is served");
  let root = mount(&mut stream, &capability, 1);
  let file = lookup(&mut stream, &root, "from-guest.txt", 2);
  let got = read(&mut stream, &file, 3);
  assert_eq!(
    got, PAYLOAD,
    "the host reads back over NFS the bytes the guest wrote through virtqueues"
  );

  let capabilities = guest_transport_capabilities();
  assert!(
    capabilities
      .iter()
      .any(|c| c.transport == GuestTransport::InProcess && c.supported)
  );
  assert!(capabilities.iter().all(|c| !c.dax.advertised));

  drop(stream);
  drop(client);
  drop(daemon);
}

/// §4.13: a guest whose enrolled consumer is not on the volume's access list is authenticated and
/// admitted with no rights, so its every effect is refused at the seam — a CREATE through the
/// device is answered `EPERM`, and nothing lands in the volume.
#[test]
fn a_guest_whose_consumer_has_no_rights_is_refused_every_effect() {
  let (daemon, instance) = single_shard_daemon("virtiofs-foreign");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("guest")) else {
    panic!("the volume was not created");
  };
  let foreign = Principal::Uid {
    uid: my_uid().wrapping_add(1),
  };
  let (outcome, error) = run_guest(&daemon, id, foreign, |kick_write, call_read| {
    Box::pin(async move {
      let created = round_trip(
        &kick_write,
        &call_read,
        &message(Opcode::Create.to_wire(), 1, 1, &create_body("intruder.txt")),
      )
      .await;
      drop(kick_write);
      reply_error(&created)
    })
  });
  assert_eq!(
    error, -EPERM,
    "the access list refused the foreign consumer's create"
  );
  assert!(matches!(outcome, GuestDeviceOutcome::Ended(_)));
  drop(client);
  drop(daemon);
}

/// AUD-29-82 (D-18, §4.8). Do: under a daemon whose anchor segment the test holds, a guest creates a file,
/// writes bytes and closes it (FLUSH, then RELEASE) through virtqueues, every reply a success; stop the daemon
/// — a stop publishes nothing — and start a second one over the same segment. Expect: the second daemon reads
/// back the guest's bytes over NFS, because the guest learned of the CREATE and the FLUSH only after the
/// owner's barrier captured the volume; the first daemon refused no barrier. Before, the guest path never
/// published, so the acknowledged file was lost at the restart.
#[test]
fn a_guests_acknowledged_close_survives_a_daemon_restart() {
  let profile = common::machine_profile();
  let instance = format!("srv-guest-durable-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let segment = anchor_segment("guest-durable", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("durable")) else {
    panic!("the volume was not created");
  };
  let (outcome, errors) = run_guest(
    &first,
    id,
    Principal::Uid { uid: my_uid() },
    |kick_write, call_read| {
      Box::pin(async move {
        let created = round_trip(
          &kick_write,
          &call_read,
          &message(Opcode::Create.to_wire(), 1, 1, &create_body("kept.txt")),
        )
        .await;
        let ino = u64_at(&created, OUT_HEADER_LEN);
        let fh = u64_at(&created, OUT_HEADER_LEN + EntryOut::LEN);
        let mut errors = vec![reply_error(&created)];
        for (unique, opcode, body) in [
          (2, Opcode::Write, write_body(fh, PAYLOAD)),
          (3, Opcode::Flush, flush_body(fh)),
          (4, Opcode::Release, release_body(fh)),
        ] {
          let reply = round_trip(
            &kick_write,
            &call_read,
            &message(opcode.to_wire(), unique, ino, &body),
          )
          .await;
          errors.push(reply_error(&reply));
        }
        drop(kick_write);
        errors
      })
    },
  );
  assert_eq!(
    errors,
    [0, 0, 0, 0],
    "create, write, flush and release succeeded"
  );
  assert!(
    matches!(outcome, GuestDeviceOutcome::Ended(_)),
    "{outcome:?}"
  );
  let refusals = first.refusals_on_every_shard().unwrap();
  assert_eq!(
    refusals.get("virtiofs.barrier_refused"),
    None,
    "{refusals:?}"
  );
  drop(client);
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let capability = second
    .mount_capability("durable")
    .expect("the owner shard answers")
    .expect("the volume was recovered");
  let port = second.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root = mount(&mut stream, &capability, 1);
  let file = lookup(&mut stream, &root, "kept.txt", 2);
  assert_eq!(
    read(&mut stream, &file, 3),
    PAYLOAD,
    "the bytes the guest closed survived the restart"
  );
  drop(stream);
  second.stop();
  drop(segment);
}

/// Shape: heartbeats the guest watches its requests after the revocation — several of the loop's pass
/// boundaries, so a loop that went on serving would have answered them.
const REVOKED_WATCH_HEARTBEATS: u64 = 5;

/// AUD-29-73 (§4.13 "every later effect"). Do: the human enrolls a consumer and shares a volume with it; a
/// guest authenticated as that consumer reads through its device; the human revokes the consumer; the guest
/// then submits a read and a create and watches them for several heartbeats. Expect: the first read answered;
/// `Revoked` acknowledged; neither later request answered; the device loop ended `Revoked` through its terminal
/// step, its references swept. Before, the revocation marked only the consumer's channels, and the device went
/// on serving.
#[test]
fn a_consumers_revocation_stops_its_guest_device() {
  let (daemon, instance) = single_shard_daemon("guest-revoke");
  let secret = daemon.segment().issuer_secret().unwrap();
  let account = my_uid();
  let mut owner = Client::connect(&instance);
  let ReplyBody::Enrolled { consumer, .. } = owner.call(&RequestBody::Enroll {
    account,
    proof: slates_server::landing::enroll_proof(&secret, account),
  }) else {
    panic!("the consumer was not enrolled");
  };
  let ReplyBody::Created { id } = owner.call(&scratch("guest-revoked")) else {
    panic!("the volume was not created");
  };
  assert!(matches!(
    owner.call(&RequestBody::Share {
      volume: id,
      principal: slates_ipc::protocol::Principal::Consumer { account, consumer },
      rights: slates_ipc::protocol::Rights {
        read: true,
        write: true,
        admin: false
      },
    }),
    ReplyBody::Shared
  ));
  let (first_tx, first_rx) = channel::<i32>();
  let (revoked_tx, revoked_rx) = channel::<()>();
  let guest = common::guest::start_guest(
    &daemon,
    id,
    Principal::Consumer { account, consumer },
    slates_server::virtiofs::GuestView::default(),
    move |kick_write, call_read| {
      Box::pin(async move {
        let first = round_trip(
          &kick_write,
          &call_read,
          &message(Opcode::GetAttr.to_wire(), 1, 1, &[0u8; 16]),
        )
        .await;
        let _ = first_tx.send(reply_error(&first));
        while revoked_rx.try_recv().is_err() {
          slates_rt::futures::sleep(slates_server::daemon::HEARTBEAT_NS)
            .await
            .unwrap();
        }
        common::guest::with_guest(|g| {
          g.submit(
            &message(Opcode::GetAttr.to_wire(), 2, 1, &[0u8; 16]),
            common::guest::REPLY_CAP,
          );
          g.submit(
            &message(Opcode::Create.to_wire(), 3, 1, &create_body("after-revoke")),
            common::guest::REPLY_CAP,
          );
        });
        let _ = rustix::io::write(&kick_write, &[1u8]);
        slates_rt::futures::sleep(slates_server::daemon::HEARTBEAT_NS * REVOKED_WATCH_HEARTBEATS)
          .await
          .unwrap();
        let answered = common::guest::with_guest(|g| g.reap()).is_some();
        drop(kick_write);
        answered
      })
    },
  );
  let first = first_rx.recv_timeout(common::guest::WAIT);
  let revoked = owner.call(&RequestBody::Revoke {
    consumer,
    proof: slates_server::landing::revoke_proof(&secret, consumer),
  });
  let _ = revoked_tx.send(());
  let answered = guest.script.recv_timeout(common::guest::WAIT);
  let outcome = guest.end.recv_timeout(common::guest::WAIT);
  drop(owner);
  drop(daemon);
  assert_eq!(
    first,
    Ok(0),
    "the consumer's guest read before the revocation"
  );
  assert_eq!(revoked, ReplyBody::Revoked);
  assert_eq!(
    answered,
    Ok(false),
    "no request answered after the revocation"
  );
  let Ok(GuestDeviceOutcome::Ended(end)) = outcome else {
    panic!("the device loop did not end: {outcome:?}");
  };
  assert_eq!(end.why, EndReason::Revoked);
  assert!(
    end.reclaimed.as_ref().is_ok_and(|r| r.references_swept),
    "{end:?}"
  );
}

/// Format: `ENOENT`.
const ENOENT: i32 = 2;

/// A LOOKUP of `name` in node `parent`: the request bytes.
fn lookup_message(unique: u64, parent: u64, name: &str) -> Vec<u8> {
  let mut body = name.as_bytes().to_vec();
  body.push(0);
  message(Opcode::Lookup.to_wire(), unique, parent, &body)
}

/// A GETATTR of node `node` (`fuse_getattr_in`: flags, a padding word, fh — all zero).
fn getattr_message(unique: u64, node: u64) -> Vec<u8> {
  message(Opcode::GetAttr.to_wire(), unique, node, &[0u8; 16])
}

/// The inode an NFS handle names.
fn inode_of(fh: &[u8]) -> u64 {
  use slates_bridge_nfs::handle::FileHandle;
  FileHandle::from_fh(&slates_bridge_nfs::nfs::Nfsfh3(fh.to_vec()))
    .unwrap()
    .inode
}

/// Through the owner's NFS mount of volume `name`: `shared/g` and `private/secret`, `f` holding `before`;
/// a snapshot taken; then `f` rewritten `after!!`. The snapshot and `private`'s inode.
fn shared_private_and_a_snapshot(
  daemon: &Daemon,
  client: &mut Client,
  id: slates_ipc::protocol::VolumeId,
  name: &str,
) -> (slates_ipc::protocol::SnapshotId, u64) {
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon.mount_capability(name).unwrap().unwrap();
  let root = mount(&mut stream, &capability, 1);
  let shared = common::nfs::mkdir(&mut stream, &root, "shared", 2);
  let private = common::nfs::mkdir(&mut stream, &root, "private", 3);
  common::nfs::create(&mut stream, &shared, "g", 4);
  common::nfs::create(&mut stream, &private, "secret", 5);
  let file = common::nfs::create(&mut stream, &root, "f", 6);
  common::nfs::write(&mut stream, &file, b"before", 7);
  let ReplyBody::Snapshotted { id: snapshot, .. } =
    client.call(&RequestBody::Snapshot { volume: id })
  else {
    panic!("snapshot");
  };
  common::nfs::write(&mut stream, &file, b"after!!", 8);
  (snapshot, inode_of(&private))
}

/// The errors a scoped guest meets: LOOKUP `g` at its root, LOOKUP `private` there, GETATTR of `private`'s
/// inode named directly.
async fn scoped_guest_script(
  kick_write: std::os::fd::OwnedFd,
  call_read: std::os::fd::OwnedFd,
  private: u64,
) -> (i32, i32, i32) {
  let inside = round_trip(&kick_write, &call_read, &lookup_message(1, 1, "g")).await;
  let sibling = round_trip(&kick_write, &call_read, &lookup_message(2, 1, "private")).await;
  let forged = round_trip(&kick_write, &call_read, &getattr_message(3, private)).await;
  drop(kick_write);
  (
    reply_error(&inside),
    reply_error(&sibling),
    reply_error(&forged),
  )
}

/// What a snapshot guest meets: `f`'s size at its root, and the error of a CREATE.
async fn snapshot_guest_script(
  kick_write: std::os::fd::OwnedFd,
  call_read: std::os::fd::OwnedFd,
) -> (i32, u64, i32) {
  let found = round_trip(&kick_write, &call_read, &lookup_message(1, 1, "f")).await;
  let created = round_trip(
    &kick_write,
    &call_read,
    &message(Opcode::Create.to_wire(), 2, 1, &create_body("new")),
  )
  .await;
  drop(kick_write);
  // EntryOut: nodeid, generation, two validities, two nanosecond words (40 bytes), then fuse_attr: ino, size.
  let size = u64_at(&found, OUT_HEADER_LEN + 48);
  (reply_error(&found), size, reply_error(&created))
}

/// AUD-29-76 (a guest's view). Do: make `shared/g`, `private/secret` and `f` (`before`), snapshot, rewrite `f`
/// `after!!`; attach one guest device presenting `/shared` and another presenting the snapshot. Through the
/// scoped device look up `g` and `private` at its root and GETATTR `private`'s inode directly; through the
/// snapshot device look up `f` and CREATE a file; ask for a subtree of a snapshot. Expect: the scoped device
/// finds `g` and answers `ENOENT` for `private` both ways; the snapshot device sees `f` at the snapshot's six
/// bytes, not the head's seven, and refuses the CREATE (`EPERM`); a subtree of a snapshot is refused typed with
/// nothing admitted. Before 2026-10-01 a guest device presented only the volume's head, whole.
#[test]
fn a_guest_device_presents_a_subtree_or_a_snapshot_and_nothing_else() {
  let (daemon, instance) = single_shard_daemon("virtiofs-view");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("viewed")) else {
    panic!("the volume was not created");
  };
  let (snapshot, private) = shared_private_and_a_snapshot(&daemon, &mut client, id, "viewed");
  let me = Principal::Uid { uid: my_uid() };
  let subtree = slates_server::virtiofs::GuestView {
    subtree: Some("/shared".to_owned()),
    snapshot: None,
  };
  let (outcome, errors) = run_guest_viewing(&daemon, id, me.clone(), subtree, move |kick, call| {
    Box::pin(scoped_guest_script(kick, call, private))
  });
  assert!(
    matches!(outcome, GuestDeviceOutcome::Ended(_)),
    "{outcome:?}"
  );
  assert_eq!(errors, (0, -ENOENT, -ENOENT), "g found; private nowhere");
  assert_the_snapshot_device_is_read_only(&daemon, id, me.clone(), snapshot);
  let both = slates_server::virtiofs::GuestView {
    subtree: Some("/shared".to_owned()),
    snapshot: Some(snapshot),
  };
  let (outcome, ()) = run_guest_viewing(&daemon, id, me, both, |kick, _call| {
    Box::pin(async move { drop(kick) })
  });
  assert!(
    matches!(outcome, GuestDeviceOutcome::ViewRefused(_)),
    "{outcome:?}"
  );
  assert!(
    matches!(
      client.call(&RequestBody::DestroySnapshot {
        volume: id,
        snapshot
      }),
      ReplyBody::SnapshotDestroyed
    ),
    "every guest view closed with its device, unpinning the snapshot"
  );
  drop(client);
  drop(daemon);
}

/// A guest device presenting `snapshot` of volume `id` finds `f` at the snapshot's six bytes, not the head's
/// seven, and its CREATE is refused `EPERM`.
fn assert_the_snapshot_device_is_read_only(
  daemon: &Daemon,
  id: slates_ipc::protocol::VolumeId,
  me: Principal,
  snapshot: slates_ipc::protocol::SnapshotId,
) {
  let pinned = slates_server::virtiofs::GuestView {
    subtree: None,
    snapshot: Some(snapshot),
  };
  let (outcome, (found, size, created)) =
    run_guest_viewing(daemon, id, me, pinned, |kick, call| {
      Box::pin(snapshot_guest_script(kick, call))
    });
  assert!(
    matches!(outcome, GuestDeviceOutcome::Ended(_)),
    "{outcome:?}"
  );
  assert_eq!(
    (found, size),
    (0, 6),
    "the snapshot's `before`, not the head"
  );
  assert_eq!(created, -EPERM, "a snapshot device writes nothing");
}

/// A vhost-user front end, as a VMM speaks it (the inherited-descriptor form; AUD-29-68): the protocol over one
/// end of a socketpair, the guest's memory a sealed memory object, the kicks and calls eventfds. Linux only.
#[cfg(target_os = "linux")]
mod vhost_front_end {
  #![allow(clippy::disallowed_methods)] // memory objects and eventfds: RAM, not a host path (R1).
  use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

  use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll};
  use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvFlags, SendAncillaryBuffer, SendAncillaryMessage,
    SendFlags, SocketFlags, SocketType,
  };
  use slates_bridge_virtiofs::vhost_user::{
    FLAG_NEED_REPLY, GuestRegions, HEADER_LEN, MappedRegion, OFFERED_FEATURES,
    OFFERED_PROTOCOL_FEATURES, RegionEntry, VERSION, request,
  };

  use super::common::guest::{GUEST_RAM, Guest};

  /// Shape: where the front end's own mapping of guest memory sits in its address space (any value: the
  /// back end translates ring addresses from it).
  pub(crate) const USER_BASE: u64 = 0x7f00_0000_0000;
  /// Shape: how long the front end waits for one reply or one interrupt.
  const WAIT_NS: i64 = 10_000_000_000;

  /// A sealed memory object of the guest's RAM, or (with `seal` false) one that may still shrink.
  pub(crate) fn guest_ram(seal: bool) -> OwnedFd {
    use rustix::fs::{MemfdFlags, SealFlags};
    let fd = rustix::fs::memfd_create("guest-ram", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)
      .unwrap();
    rustix::fs::ftruncate(&fd, GUEST_RAM).unwrap();
    if seal {
      rustix::fs::fcntl_add_seals(&fd, SealFlags::SHRINK | SealFlags::GROW).unwrap();
    }
    fd
  }

  /// The guest's driver over a second mapping of `ram`.
  pub(crate) fn guest_over(ram: &OwnedFd) -> Guest<GuestRegions> {
    let region = MappedRegion::map(region_entry(), ram).unwrap();
    Guest::over(GuestRegions::new(vec![region]).unwrap())
  }

  fn region_entry() -> RegionEntry {
    RegionEntry {
      guest_phys: 0,
      size: GUEST_RAM,
      user_addr: USER_BASE,
      offset: 0,
    }
  }

  /// The front end: its socket, and per queue its kick and call eventfds.
  pub(crate) struct FrontEnd {
    socket: OwnedFd,
    pub(crate) kicks: Vec<OwnedFd>,
    pub(crate) calls: Vec<OwnedFd>,
  }

  /// A connected pair: the front end, and the end the daemon adopts.
  pub(crate) fn pair() -> (FrontEnd, OwnedFd) {
    let (ours, theirs) = rustix::net::socketpair(
      AddressFamily::UNIX,
      SocketType::STREAM,
      SocketFlags::CLOEXEC,
      None,
    )
    .unwrap();
    let eventfds = || {
      (0..2)
        .map(|_| eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).unwrap())
        .collect()
    };
    (
      FrontEnd {
        socket: ours,
        kicks: eventfds(),
        calls: eventfds(),
      },
      theirs,
    )
  }

  impl FrontEnd {
    fn send(&self, request: u32, flags: u32, payload: &[u8], fds: &[BorrowedFd<'_>]) {
      let mut message = Vec::new();
      message.extend_from_slice(&request.to_le_bytes());
      message.extend_from_slice(&(VERSION | flags).to_le_bytes());
      message.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
      message.extend_from_slice(payload);
      let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(8))];
      let mut control = SendAncillaryBuffer::new(&mut space);
      if !fds.is_empty() {
        assert!(control.push(SendAncillaryMessage::ScmRights(fds)));
      }
      let sent = rustix::net::sendmsg(
        &self.socket,
        &[std::io::IoSlice::new(&message)],
        &mut control,
        SendFlags::NOSIGNAL,
      )
      .unwrap();
      assert_eq!(sent, message.len());
    }

    /// The next reply's payload, or `None` when the back end closed its end.
    pub(crate) fn reply(&self) -> Option<Vec<u8>> {
      let mut header = [0u8; HEADER_LEN];
      if !self.read_exact(&mut header) {
        return None;
      }
      let size = u32::from_le_bytes(header[8..12].try_into().unwrap());
      let mut payload = vec![0u8; usize::try_from(size).unwrap()];
      self.read_exact(&mut payload).then_some(payload)
    }

    fn read_exact(&self, out: &mut [u8]) -> bool {
      let mut at = 0;
      while at < out.len() {
        let mut fds = [PollFd::new(&self.socket, PollFlags::IN)];
        let timeout = Timespec {
          tv_sec: WAIT_NS / 1_000_000_000,
          tv_nsec: 0,
        };
        assert_eq!(
          poll(&mut fds, Some(&timeout)).unwrap(),
          1,
          "a reply in time"
        );
        let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let got = rustix::net::recvmsg(
          &self.socket,
          &mut [std::io::IoSliceMut::new(&mut out[at..])],
          &mut control,
          RecvFlags::CMSG_CLOEXEC,
        );
        match got {
          Ok(received) if received.bytes > 0 => at += received.bytes,
          _ => return false,
        }
      }
      true
    }

    /// A `GET_*` request: its `u64` answer.
    pub(crate) fn get(&self, request: u32) -> u64 {
      self.send(request, 0, &[], &[]);
      u64::from_le_bytes(self.reply().unwrap().try_into().unwrap())
    }

    /// A setting request with a reply-ack asked for: the ack's status (zero for success).
    pub(crate) fn set(&self, request: u32, payload: &[u8], fds: &[BorrowedFd<'_>]) -> u64 {
      self.send(request, FLAG_NEED_REPLY, payload, fds);
      u64::from_le_bytes(self.reply().unwrap().try_into().unwrap())
    }

    /// A vring-state request (index, number).
    pub(crate) fn state(queue: u32, number: u32) -> Vec<u8> {
      let mut payload = queue.to_le_bytes().to_vec();
      payload.extend_from_slice(&number.to_le_bytes());
      payload
    }

    /// Queues the settings and the memory table back to back, asking no reply until the table's: what QEMU
    /// sends before it waits. The table's ack, once the back end reads them.
    pub(crate) fn queue_settings_and_memory(&self, ram: &OwnedFd) {
      self.send(
        request::SET_FEATURES,
        0,
        &OFFERED_FEATURES.to_le_bytes(),
        &[],
      );
      self.send(
        request::SET_PROTOCOL_FEATURES,
        0,
        &OFFERED_PROTOCOL_FEATURES.to_le_bytes(),
        &[],
      );
      self.send(request::SET_OWNER, 0, &[], &[]);
      self.send(
        request::SET_MEM_TABLE,
        FLAG_NEED_REPLY,
        &memory_table(),
        &[ram.as_fd()],
      );
    }

    /// Negotiates features and sends the memory table of `ram` (one region from guest address zero).
    pub(crate) fn negotiate_memory(&self, ram: &OwnedFd) -> u64 {
      assert_eq!(self.get(request::GET_FEATURES), OFFERED_FEATURES);
      self.send(
        request::SET_FEATURES,
        0,
        &OFFERED_FEATURES.to_le_bytes(),
        &[],
      );
      assert_eq!(
        self.get(request::GET_PROTOCOL_FEATURES),
        OFFERED_PROTOCOL_FEATURES
      );
      self.send(
        request::SET_PROTOCOL_FEATURES,
        0,
        &OFFERED_PROTOCOL_FEATURES.to_le_bytes(),
        &[],
      );
      assert_eq!(self.get(request::GET_QUEUE_NUM), 2);
      assert_eq!(self.set(request::SET_OWNER, &[], &[]), 0);
      self.set(request::SET_MEM_TABLE, &memory_table(), &[ram.as_fd()])
    }

    /// Configures each queue of `guest` and enables it; every ack must report success.
    pub(crate) fn configure_queues<M: slates_bridge_virtiofs::memory::GuestMemory>(
      &self,
      guest: &Guest<M>,
    ) {
      for (queue, layout) in guest.layouts().iter().enumerate() {
        let index = u32::try_from(queue).unwrap();
        let acks = [
          self.set(
            request::SET_VRING_NUM,
            &Self::state(index, u32::from(layout.size)),
            &[],
          ),
          self.set(request::SET_VRING_ADDR, &ring_addresses(index, layout), &[]),
          self.set(request::SET_VRING_BASE, &Self::state(index, 0), &[]),
          self.set(
            request::SET_VRING_KICK,
            &u64::from(index).to_le_bytes(),
            &[self.kicks[queue].as_fd()],
          ),
          self.set(
            request::SET_VRING_CALL,
            &u64::from(index).to_le_bytes(),
            &[self.calls[queue].as_fd()],
          ),
          self.set(request::SET_VRING_ENABLE, &Self::state(index, 1), &[]),
        ];
        assert_eq!(acks, [0; 6], "queue {queue} configured");
      }
    }

    /// Kicks `queue` and waits for its interrupt.
    pub(crate) fn kick_and_wait(&self, queue: usize) {
      rustix::io::write(&self.kicks[queue], &1u64.to_le_bytes()).unwrap();
      let mut fds = [PollFd::new(&self.calls[queue], PollFlags::IN)];
      let timeout = Timespec {
        tv_sec: WAIT_NS / 1_000_000_000,
        tv_nsec: 0,
      };
      assert_eq!(
        poll(&mut fds, Some(&timeout)).unwrap(),
        1,
        "an interrupt in time"
      );
      let mut counter = [0u8; 8];
      rustix::io::read(&self.calls[queue], &mut counter).unwrap();
    }

    /// `GET_VRING_BASE` of `queue`: the ring stops; its position.
    pub(crate) fn stop(&self, queue: u32) -> u32 {
      self.send(request::GET_VRING_BASE, 0, &Self::state(queue, 0), &[]);
      let reply = self.reply().unwrap();
      u32::from_le_bytes(reply[4..8].try_into().unwrap())
    }
  }

  /// The memory table: one region, the guest's RAM from guest address zero.
  fn memory_table() -> Vec<u8> {
    let entry = region_entry();
    let mut table = 1u32.to_le_bytes().to_vec();
    table.extend_from_slice(&0u32.to_le_bytes());
    for word in [entry.guest_phys, entry.size, entry.user_addr, entry.offset] {
      table.extend_from_slice(&word.to_le_bytes());
    }
    table
  }

  /// The reply-ack's status from a reply payload.
  pub(crate) fn status(reply: &[u8]) -> u64 {
    u64::from_le_bytes(reply.try_into().unwrap())
  }

  /// A vring-address payload for `layout` in the front end's address space.
  fn ring_addresses(
    index: u32,
    layout: &slates_bridge_virtiofs::virtqueue::QueueLayout,
  ) -> Vec<u8> {
    let mut payload = index.to_le_bytes().to_vec();
    payload.extend_from_slice(&0u32.to_le_bytes());
    for at in [
      layout.descriptor_table.0,
      layout.used_ring.0,
      layout.available_ring.0,
      0,
    ] {
      let user = if at == 0 { 0 } else { USER_BASE + at };
      payload.extend_from_slice(&user.to_le_bytes());
    }
    payload
  }
}

/// Shape: the test front end's handshake bound: it configures the rings at once, so its own reply wait.
#[cfg(target_os = "linux")]
const HANDSHAKE_NS: u64 = 10_000_000_000;

/// One FUSE request through the vhost-user guest: submitted on the request queue, kicked, its interrupt
/// awaited, its reply read back.
#[cfg(target_os = "linux")]
fn through_vhost(
  front: &vhost_front_end::FrontEnd,
  guest: &mut common::guest::Guest<slates_bridge_virtiofs::vhost_user::GuestRegions>,
  request: &[u8],
) -> Vec<u8> {
  let head = guest.submit(request, common::guest::REPLY_CAP);
  front.kick_and_wait(1);
  let (id, len) = guest.reap().expect("a used element after the interrupt");
  assert_eq!(id, head);
  guest.reply_of(head, len)
}

/// Attaches a vhost-user device on `id` over `socket`; the receiver of its outcome.
#[cfg(target_os = "linux")]
fn attach_vhost(
  daemon: &Daemon,
  id: slates_ipc::protocol::VolumeId,
  (socket, handshake_ns): (std::os::fd::OwnedFd, u64),
) -> std::sync::mpsc::Receiver<GuestDeviceOutcome> {
  let (end_tx, end_rx) = channel();
  daemon
    .attach_vhost_user_device(
      id,
      slates_bridge_virtiofs::device::FsTag::new("slates").unwrap(),
      slates_server::virtiofs::GuestView::default(),
      (socket, handshake_ns),
      slates_server::virtiofs::GuestHarness {
        on_admitted: Box::new(|_| {}),
        on_end: Box::new(move |outcome| {
          let _ = end_tx.send(outcome);
        }),
      },
    )
    .unwrap();
  end_rx
}

/// AUD-29-68 (the inherited-descriptor binding). Do: provision a volume; hand the daemon one end of a
/// socketpair as a vhost-user device; as the VMM, negotiate features, send a sealed memory object as the
/// guest's RAM and configure both queues with eventfd kicks and calls; as the guest, CREATE, WRITE and RELEASE
/// a file through the request queue, each kicked and its interrupt awaited; stop the ring (`GET_VRING_BASE`);
/// read the file back over the daemon's NFS port. Expect: every acknowledgement reports success; each request
/// is answered through guest memory and its interrupt raised on the call eventfd; the stopped ring reports
/// position 3 (three requests consumed); the device then ends through its terminal step with the doorbell hung
/// up; the host reads the guest's bytes. Before 2026-10-01 this form was refused `BindingNotBuilt`.
#[cfg(target_os = "linux")]
#[test]
fn a_vhost_user_front_end_drives_a_guest_whose_file_the_host_reads_back() {
  let (daemon, instance) = single_shard_daemon("virtiofs-vhost");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("vhosted")) else {
    panic!("the volume was not created");
  };
  let (front, socket) = vhost_front_end::pair();
  let ended = attach_vhost(&daemon, id, (socket, HANDSHAKE_NS));
  let ram = vhost_front_end::guest_ram(true);
  let mut guest = vhost_front_end::guest_over(&ram);
  assert_eq!(
    front.negotiate_memory(&ram),
    0,
    "the memory table is accepted"
  );
  front.configure_queues(&guest);
  let created = through_vhost(
    &front,
    &mut guest,
    &message(
      Opcode::Create.to_wire(),
      1,
      1,
      &create_body("from-vhost.txt"),
    ),
  );
  assert_eq!(reply_error(&created), 0);
  let ino = u64_at(&created, OUT_HEADER_LEN);
  let fh = u64_at(&created, OUT_HEADER_LEN + EntryOut::LEN);
  let written = through_vhost(
    &front,
    &mut guest,
    &message(Opcode::Write.to_wire(), 2, ino, &write_body(fh, PAYLOAD)),
  );
  assert_eq!(reply_error(&written), 0);
  let released = through_vhost(
    &front,
    &mut guest,
    &message(Opcode::Release.to_wire(), 3, ino, &release_body(fh)),
  );
  assert_eq!(reply_error(&released), 0);
  assert_eq!(front.stop(1), 3, "the ring's position is what it consumed");
  let outcome = ended
    .recv_timeout(common::guest::WAIT)
    .expect("the device ended");
  let GuestDeviceOutcome::Ended(end) = outcome else {
    panic!("the device did not serve: {outcome:?}");
  };
  assert_eq!(end.why, EndReason::DoorbellHungUp);
  assert!(
    end
      .reclaimed
      .expect("the terminal step ran")
      .references_swept
  );
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon.mount_capability("vhosted").unwrap().unwrap();
  let root = mount(&mut stream, &capability, 1);
  let file = lookup(&mut stream, &root, "from-vhost.txt", 2);
  assert_eq!(read(&mut stream, &file, 3), PAYLOAD);
  drop(client);
  drop(daemon);
}

/// AUD-29-68 (hostile front ends). Do: offer a memory object that is not sealed against shrinking; in a second
/// connection, close the front end before configuring any queue. Expect: the unsealed table is refused (its ack
/// reports failure) and the handshake ends refused with the memory rule named; the closed one ends refused as
/// closed; nothing is admitted either time.
#[cfg(target_os = "linux")]
#[test]
fn a_vhost_user_front_end_that_offers_unsealed_memory_or_leaves_is_refused() {
  let (daemon, instance) = single_shard_daemon("virtiofs-vhost-hostile");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("hostile")) else {
    panic!("the volume was not created");
  };
  let (front, socket) = vhost_front_end::pair();
  let ended = attach_vhost(&daemon, id, (socket, HANDSHAKE_NS));
  let unsealed = vhost_front_end::guest_ram(false);
  assert_ne!(
    front.negotiate_memory(&unsealed),
    0,
    "an unsealed table is refused"
  );
  let outcome = ended.recv_timeout(common::guest::WAIT).unwrap();
  assert!(
    matches!(
      outcome,
      GuestDeviceOutcome::HandshakeRefused(Some(
        slates_bridge_virtiofs::vhost_user::VhostError::Memory { .. }
      ))
    ),
    "{outcome:?}"
  );
  let (front, socket) = vhost_front_end::pair();
  let ended = attach_vhost(&daemon, id, (socket, HANDSHAKE_NS));
  drop(front);
  let outcome = ended.recv_timeout(common::guest::WAIT).unwrap();
  assert!(
    matches!(
      outcome,
      GuestDeviceOutcome::HandshakeRefused(Some(
        slates_bridge_virtiofs::vhost_user::VhostError::Closed
      ))
    ),
    "{outcome:?}"
  );
  drop(client);
  drop(daemon);
}

/// Shape: how long the live guest may take from QEMU's start to its power-off — the boot, the mount, a few
/// file calls — under software emulation; measured at about 2 s on an Apple Silicon Docker Desktop VM
/// (2026-10-01), so a hundredfold margin that still fails a hung guest in bounded time.
#[cfg(target_os = "linux")]
const GUEST_RUN: Duration = Duration::from_secs(200);

/// Shape: the live guest's RAM in kibibytes (`-m 256M`), the size by which its mapping is found in this
/// process's `smaps`.
#[cfg(target_os = "linux")]
const GUEST_RAM_KIB: u64 = 256 * 1024;

/// One mapping of the guest's memory object in this process (the daemon's device maps it here), as the kernel
/// accounts it: resident and locked kibibytes.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default)]
struct GuestMapping {
  rss_kib: u64,
  locked_kib: u64,
}

/// Every mapping of a memory object the size of the guest's RAM in this process (`/proc/self/smaps`: a header
/// naming `memfd:`, then its `Size`, `Rss` and `Locked` lines).
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // procfs: the kernel's account of this process, not a host path (R1).
fn guest_memory_mappings() -> Vec<GuestMapping> {
  let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap_or_default();
  let field = |line: &str, name: &str| {
    line
      .strip_prefix(name)
      .and_then(|rest| rest.trim().strip_suffix("kB"))
      .and_then(|kib| kib.trim().parse::<u64>().ok())
  };
  // A mapping's header begins with its address range (`start-end`, hex); its fields follow until the next.
  let is_header = |line: &str| {
    line.split_whitespace().next().is_some_and(|range| {
      range.contains('-') && range.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    })
  };
  let mut found = Vec::new();
  let mut current: Option<(u64, GuestMapping)> = None;
  let mut finish = |current: &mut Option<(u64, GuestMapping)>| {
    if let Some((size, mapping)) = current.take()
      && size == GUEST_RAM_KIB
    {
      found.push(mapping);
    }
  };
  for line in smaps.lines() {
    if is_header(line) {
      finish(&mut current);
      current = line
        .contains("memfd:")
        .then(|| (0, GuestMapping::default()));
      continue;
    }
    let Some((size, mapping)) = current.as_mut() else {
      continue;
    };
    if let Some(kib) = field(line, "Size:") {
      *size = kib;
    } else if let Some(kib) = field(line, "Rss:") {
      mapping.rss_kib = kib;
    } else if let Some(kib) = field(line, "Locked:") {
      mapping.locked_kib = kib;
    }
  }
  finish(&mut current);
  found
}

/// The live guest's paths, from the environment: QEMU, the guest kernel and its initramfs (whose init mounts
/// the tag `slates`, reads `from-host.txt`, writes `from-guest.txt`, makes `guest-dir`, unmounts and powers
/// off), or a loud skip.
#[cfg(target_os = "linux")]
fn live_guest() -> Option<(String, String, String)> {
  let read = |name| std::env::var(name).ok();
  match (
    read("SLATES_GUEST_QEMU"),
    read("SLATES_GUEST_KERNEL"),
    read("SLATES_GUEST_INITRD"),
  ) {
    (Some(qemu), Some(kernel), Some(initrd)) => Some((qemu, kernel, initrd)),
    _ => {
      eprintln!(
        "SKIP: the live guest needs SLATES_GUEST_QEMU, SLATES_GUEST_KERNEL and SLATES_GUEST_INITRD"
      );
      None
    }
  }
}

/// Runs QEMU with a `vhost-user-fs-pci` device whose front end speaks over `socket` (inherited at its own
/// number), the guest's RAM a sealed memfd; its console output once it exits, or the run's failure.
#[cfg(target_os = "linux")]
fn run_qemu(
  guest: &(String, String, String),
  socket: &std::os::fd::OwnedFd,
  sample: &mut dyn FnMut(&[GuestMapping]),
) -> String {
  run_qemu_with(guest, socket, (GUEST_RUN, "", false), sample)
}

/// The QEMU machine a live guest runs on this host's architecture, and whether KVM accelerates it.
#[cfg(target_os = "linux")]
struct GuestMachine {
  /// The machine type, with the memory backend the vhost-user device needs shared.
  machine: &'static str,
  /// The guest kernel's console device.
  console: &'static str,
  /// Whether `/dev/kvm` is usable here (the CI runner; Docker Desktop on Apple Silicon has none).
  kvm: bool,
}

#[cfg(target_os = "linux")]
impl GuestMachine {
  fn of_this_host() -> GuestMachine {
    let kvm = rustix::fs::access(
      "/dev/kvm",
      rustix::fs::Access::READ_OK | rustix::fs::Access::WRITE_OK,
    )
    .is_ok();
    if cfg!(target_arch = "x86_64") {
      GuestMachine {
        machine: "q35,memory-backend=mem",
        console: "ttyS0",
        kvm,
      }
    } else {
      GuestMachine {
        machine: "virt,memory-backend=mem",
        console: "ttyAMA0",
        kvm,
      }
    }
  }
}

/// [`run_qemu`] within `bound`, with `extra` appended to the guest's command line and, with `host_root`, the
/// host container's root shared read-only over 9p as `hostroot`.
#[cfg(target_os = "linux")]
fn run_qemu_with(
  (qemu, kernel, initrd): &(String, String, String),
  socket: &std::os::fd::OwnedFd,
  (bound, extra, host_root): (Duration, &str, bool),
  sample: &mut dyn FnMut(&[GuestMapping]),
) -> String {
  use std::os::fd::AsRawFd;
  // The child inherits the descriptor at its own number (QEMU's `fd=` option names it).
  rustix::io::fcntl_setfd(socket, rustix::io::FdFlags::empty()).unwrap();
  let chardev = format!("socket,id=vfs,fd={}", socket.as_raw_fd());
  let machine = GuestMachine::of_this_host();
  let append = format!(
    "console={} rdinit=/init panic=-1 quiet{extra}",
    machine.console
  );
  // The workload guest compiles with rustc, which needs more than the mount guest's RAM.
  let memory = if host_root {
    WORKLOAD_GUEST_RAM
  } else {
    "256M"
  };
  let backend = format!("memory-backend-memfd,id=mem,size={memory},share=on");
  let mut command = std::process::Command::new(qemu);
  if machine.kvm {
    command.args(["-accel", "kvm"]);
  }
  if host_root {
    command.args([
      "-virtfs",
      "local,path=/,mount_tag=hostroot,security_model=none,readonly=on",
    ]);
  }
  let mut child = command
    .args([
      "-machine",
      machine.machine,
      "-cpu",
      if machine.kvm { "host" } else { "max" },
      "-smp",
      "1",
      "-m",
      memory,
      "-object",
      &backend,
      "-nographic",
      "-nic",
      "none",
      "-no-reboot",
      "-kernel",
      kernel,
      "-initrd",
      initrd,
      "-append",
      &append,
      "-chardev",
      &chardev,
      "-device",
      "vhost-user-fs-pci,chardev=vfs,tag=slates",
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .unwrap();
  let started = Instant::now();
  let mut timed_out = false;
  while child.try_wait().unwrap().is_none() {
    sample(&guest_memory_mappings());
    if started.elapsed() > bound {
      let _ = child.kill();
      timed_out = true;
      break;
    }
    // The test thread polls its child QEMU process; no runtime wheel serves this thread (D-9 allows a test).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(50));
  }
  let output = child.wait_with_output().unwrap();
  let console = format!(
    "{}{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  if timed_out {
    format!("(the guest did not power off within {bound:?})\n{console}")
  } else {
    console
  }
}

/// AUD-29-68 (a live guest). Do: provision a volume and write `from-host.txt` into it over NFS; hand the daemon
/// one end of a socketpair as a vhost-user device and QEMU the other (`-chardev socket,fd=N`, never a socket on
/// disk), its guest's RAM a sealed memfd; boot a Linux guest whose init mounts the tag over virtio-fs, reads the
/// host's file, writes `from-guest.txt`, makes `guest-dir`, unmounts and powers off; then read the volume over
/// NFS. Expect: the guest reports the host's bytes, lists its own entries, unmounts and finishes; the device
/// ends when QEMU goes; the host reads the guest's bytes and sees its directory. Gated on the live-guest
/// environment (QEMU, a kernel with virtio-fs, the initramfs); skips loudly elsewhere.
#[cfg(target_os = "linux")]
#[test]
fn a_linux_guest_mounts_the_volume_through_qemu_over_vhost_user() {
  let Some(guest) = live_guest() else {
    return;
  };
  let (daemon, instance) = single_shard_daemon("virtiofs-qemu");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("guestvol")) else {
    panic!("the volume was not created");
  };
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon.mount_capability("guestvol").unwrap().unwrap();
  let root = mount(&mut stream, &capability, 1);
  let host_file = common::nfs::create(&mut stream, &root, "from-host.txt", 2);
  common::nfs::write(&mut stream, &host_file, b"from the host", 3);
  let (ours, theirs) = rustix::net::socketpair(
    rustix::net::AddressFamily::UNIX,
    rustix::net::SocketType::STREAM,
    rustix::net::SocketFlags::CLOEXEC,
    None,
  )
  .unwrap();
  let boot_ns = u64::try_from(GUEST_RUN.as_nanos()).unwrap();
  let ended = attach_vhost(&daemon, id, (ours, boot_ns));
  let console = run_and_measure(&guest, theirs);
  if !console.contains("SLATES-GUEST-OK") {
    let outcome = ended.recv_timeout(common::guest::WAIT);
    panic!("the guest did not finish; the device: {outcome:?}\n{console}");
  }
  assert!(
    console.contains("SLATES-HOST-SAYS: from the host"),
    "{console}"
  );
  assert!(console.contains("SLATES-GUEST-UNMOUNTED"), "{console}");
  assert!(console.contains("SLATES-GUEST-OK"), "{console}");
  let outcome = ended
    .recv_timeout(common::guest::WAIT)
    .expect("the device ended");
  assert!(
    matches!(outcome, GuestDeviceOutcome::Ended(_)),
    "{outcome:?}"
  );
  let guest_file = lookup(&mut stream, &root, "from-guest.txt", 4);
  assert_eq!(read(&mut stream, &guest_file, 5), b"written by the guest\n");
  assert!(common::nfs::lookup_status(&mut stream, &root, "guest-dir", 6).is_ok());
  drop(client);
  drop(daemon);
}

/// AUD-29-68 (messages queued back to back). Do: as the front end, queue `SET_FEATURES`,
/// `SET_PROTOCOL_FEATURES`, `SET_OWNER` and `SET_MEM_TABLE` (the last carrying the memory object, asking a
/// reply-ack) before the daemon adopts the socket, so its first reads find them all waiting — what QEMU sends
/// before it waits. Expect: the table is accepted (ack 0). Before the fix one receive took several messages,
/// the table's descriptor arrived while an earlier message was served and was closed with it, and the table
/// was refused (`a memory table whose descriptors do not match its regions`; QEMU: `vhost_set_mem_table failed`).
#[cfg(target_os = "linux")]
#[test]
fn a_vhost_user_front_end_sending_back_to_back_has_each_descriptor_kept_with_its_message() {
  let (daemon, instance) = single_shard_daemon("virtiofs-vhost-queued");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("queued")) else {
    panic!("the volume was not created");
  };
  let (front, socket) = vhost_front_end::pair();
  let ram = vhost_front_end::guest_ram(true);
  front.queue_settings_and_memory(&ram);
  let _ended = attach_vhost(&daemon, id, (socket, HANDSHAKE_NS));
  let reply = front.reply().expect("the table's ack");
  assert_eq!(
    vhost_front_end::status(&reply),
    0,
    "the memory table is accepted"
  );
  drop(front);
  drop(client);
  drop(daemon);
}

/// The volume's attachment count as `status` reports it.
fn attachments_of(client: &mut Client, volume: slates_ipc::protocol::VolumeId) -> u32 {
  let ReplyBody::Status { report } = client.call(&RequestBody::Status { volume }) else {
    panic!("status");
  };
  report.attachments
}

/// Through the owner's NFS mount of volume `name`: `f` holding three bytes, a snapshot, five bytes, a snapshot,
/// seven bytes. The two snapshots.
fn three_versions_of_f(
  daemon: &Daemon,
  client: &mut Client,
  id: slates_ipc::protocol::VolumeId,
  name: &str,
) -> [slates_ipc::protocol::SnapshotId; 2] {
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon.mount_capability(name).unwrap().unwrap();
  let root = mount(&mut stream, &capability, 1);
  let file = common::nfs::create(&mut stream, &root, "f", 2);
  let mut snapshots = Vec::new();
  for (xid, bytes) in [(3, &b"one"[..]), (4, b"three")] {
    common::nfs::write(&mut stream, &file, bytes, xid);
    let ReplyBody::Snapshotted { id: snapshot, .. } =
      client.call(&RequestBody::Snapshot { volume: id })
    else {
      panic!("snapshot");
    };
    snapshots.push(snapshot);
  }
  common::nfs::write(&mut stream, &file, b"seven!!", 5);
  snapshots.try_into().unwrap()
}

/// Waits on the guest's side (the owning shard's thread) until `signal` arrives, a heartbeat at a time.
async fn until(signal: std::sync::mpsc::Receiver<()>) {
  while signal.try_recv().is_err() {
    slates_rt::futures::sleep(slates_server::daemon::HEARTBEAT_NS)
      .await
      .unwrap();
  }
}

/// The guest's script: `f`'s size at its root, reported; after `advanced`, the size again, reported; after
/// `detached`, a GETATTR submitted and kicked, and whether it was answered within five heartbeats.
fn advance_and_detach_script(
  sizes: std::sync::mpsc::Sender<u64>,
  advanced: std::sync::mpsc::Receiver<()>,
  detached: std::sync::mpsc::Receiver<()>,
) -> impl FnOnce(
  std::os::fd::OwnedFd,
  std::os::fd::OwnedFd,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> {
  move |kick_write, call_read| {
    Box::pin(async move {
      // EntryOut: nodeid, generation, two validities, two nanosecond words, then fuse_attr: ino, size.
      let size_at = |reply: &[u8]| u64_at(reply, OUT_HEADER_LEN + 48);
      let first = round_trip(&kick_write, &call_read, &lookup_message(1, 1, "f")).await;
      let _ = sizes.send(size_at(&first));
      until(advanced).await;
      let second = round_trip(&kick_write, &call_read, &lookup_message(2, 1, "f")).await;
      let _ = sizes.send(size_at(&second));
      until(detached).await;
      common::guest::with_guest(|g| {
        g.submit(&getattr_message(3, 1), common::guest::REPLY_CAP);
      });
      let _ = rustix::io::write(&kick_write, &[1u8]);
      slates_rt::futures::sleep(slates_server::daemon::HEARTBEAT_NS * REVOKED_WATCH_HEARTBEATS)
        .await
        .unwrap();
      let answered = common::guest::with_guest(|g| g.reap()).is_some();
      drop(kick_write);
      answered
    })
  }
}

/// AUD-29-68 (the guest's durable record). Do: write `f` at three, five and seven bytes with a snapshot after the
/// first two; attach a guest device presenting the first snapshot; read `status`; look `f` up in the guest;
/// `advance` the device's attachment to the second snapshot and look `f` up again; `detach` the attachment and
/// send the guest one more request; destroy both snapshots. Expect: the harness learns the device's record id;
/// `status` counts it while it lives; the guest sees three bytes, then five after the advance, which names
/// `/f`; the detach ends the device (`Revoked`, references swept) and its request goes unanswered; `status`
/// counts it no longer; both snapshots are then destroyable (no view pins them). Before 2026-10-01 a guest
/// device had no record: `status` could not see it, `detach` could not end it and its view could not move.
#[test]
fn a_guest_device_is_recorded_its_snapshot_view_advances_and_its_detach_ends_it() {
  let (daemon, instance) = single_shard_daemon("virtiofs-recorded");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("recorded")) else {
    panic!("the volume was not created");
  };
  let [first, second] = three_versions_of_f(&daemon, &mut client, id, "recorded");
  let before = attachments_of(&mut client, id);
  let (sizes_tx, sizes) = channel::<u64>();
  let (advanced_tx, advanced_rx) = channel::<()>();
  let (detached_tx, detached_rx) = channel::<()>();
  let guest = common::guest::start_guest(
    &daemon,
    id,
    Principal::Uid { uid: my_uid() },
    slates_server::virtiofs::GuestView {
      subtree: None,
      snapshot: Some(first),
    },
    advance_and_detach_script(sizes_tx, advanced_rx, detached_rx),
  );
  let attachment = guest
    .admitted
    .recv_timeout(common::guest::WAIT)
    .expect("the device was admitted");
  assert_eq!(
    attachments_of(&mut client, id),
    before + 1,
    "the device is recorded"
  );
  assert_the_view_advances(&mut client, attachment, second, (&sizes, advanced_tx));
  assert_the_detach_ends_the_device(&mut client, attachment, &guest, detached_tx);
  assert_eq!(attachments_of(&mut client, id), before);
  for snapshot in [first, second] {
    assert!(matches!(
      client.call(&RequestBody::DestroySnapshot {
        volume: id,
        snapshot
      }),
      ReplyBody::SnapshotDestroyed
    ));
  }
  drop(client);
  drop(daemon);
}

/// AUD-29-68 (a guest's record outlives a dead daemon only until recovery). Do: under a daemon whose anchor
/// segment the test holds, attach a guest device and wait for its record; stop the daemon with the device still
/// attached (its loop is dropped unended, as a crash drops it; a stop publishes nothing); start a second daemon
/// over the same segment and read `status`. Expect: the first daemon counts the device; the second counts it no
/// longer — its device and seam were the dead process's own, so recovery ended the record rather than leaving
/// an attachment nothing serves.
#[test]
fn a_dead_daemons_guest_record_is_ended_by_recovery() {
  use common::anchor::{anchor_segment, source_of};
  let profile = common::machine_profile();
  let instance = format!("srv-guest-recorded-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let segment = anchor_segment("guest-recorded", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), source_of(&segment)).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&scratch("orphaned")) else {
    panic!("the volume was not created");
  };
  let before = attachments_of(&mut client, id);
  let (_never, waits) = channel::<()>();
  let guest = common::guest::start_guest(
    &first,
    id,
    Principal::Uid { uid: my_uid() },
    slates_server::virtiofs::GuestView::default(),
    move |kick_write, _call_read| {
      Box::pin(async move {
        until(waits).await;
        drop(kick_write);
      })
    },
  );
  guest
    .admitted
    .recv_timeout(common::guest::WAIT)
    .expect("the device was admitted");
  assert_eq!(attachments_of(&mut client, id), before + 1);
  drop(client);
  first.stop();

  let second = Daemon::start(&profile, config, source_of(&segment)).unwrap();
  let mut client = Client::connect(&instance);
  assert_eq!(
    attachments_of(&mut client, id),
    before,
    "recovery ended the dead device's record"
  );
  drop(client);
  second.stop();
  drop(segment);
}

/// The guest reads three bytes at the first snapshot; `advance` to `second` names `/f` alone; the guest then
/// reads five.
fn assert_the_view_advances(
  client: &mut Client,
  attachment: u64,
  second: slates_ipc::protocol::SnapshotId,
  (sizes, advanced): (&std::sync::mpsc::Receiver<u64>, std::sync::mpsc::Sender<()>),
) {
  assert_eq!(sizes.recv_timeout(common::guest::WAIT), Ok(3));
  let moved = client.call(&RequestBody::Advance {
    attachment,
    version: Some(second.value),
  });
  assert_eq!(
    moved,
    ReplyBody::Advanced {
      version: second.value,
      invalidated: vec!["/f".to_owned()],
    }
  );
  let _ = advanced.send(());
  assert_eq!(
    sizes.recv_timeout(common::guest::WAIT),
    Ok(5),
    "the view moved"
  );
}

/// `detach` of the device's record ends the device: its next request goes unanswered and its loop ends
/// `Revoked` with its references swept.
fn assert_the_detach_ends_the_device(
  client: &mut Client,
  attachment: u64,
  guest: &common::guest::StartedGuest<bool>,
  detached: std::sync::mpsc::Sender<()>,
) {
  assert_eq!(
    client.call(&RequestBody::Detach { attachment }),
    ReplyBody::Detached
  );
  let _ = detached.send(());
  assert_eq!(
    guest.script.recv_timeout(common::guest::WAIT),
    Ok(false),
    "no request answered after the detach"
  );
  let Ok(GuestDeviceOutcome::Ended(end)) = guest.end.recv_timeout(common::guest::WAIT) else {
    panic!("the device did not end");
  };
  assert_eq!(end.why, EndReason::Revoked);
  assert!(end.reclaimed.as_ref().is_ok_and(|r| r.references_swept));
}

/// Runs the live guest over `theirs` while sampling this process's mappings of its memory (AUD-29-77): the device
/// must have mapped it and locked none of it; the peak resident size is printed for the record. The console.
#[cfg(target_os = "linux")]
fn run_and_measure(guest: &(String, String, String), theirs: std::os::fd::OwnedFd) -> String {
  let mut peak = GuestMapping::default();
  let mut mapped = false;
  let console = run_qemu(guest, &theirs, &mut |mappings: &[GuestMapping]| {
    for mapping in mappings {
      mapped = true;
      peak.rss_kib = peak.rss_kib.max(mapping.rss_kib);
      peak.locked_kib = peak.locked_kib.max(mapping.locked_kib);
    }
  });
  drop(theirs);
  // AUD-29-77, measured: the device mapped the VMM's guest memory and locked none of it; what it touched is
  // printed for the record.
  eprintln!(
    "guest memory mapped by the device: peak resident {} KiB of {GUEST_RAM_KIB} KiB, locked {} KiB",
    peak.rss_kib, peak.locked_kib
  );
  assert!(
    mapped,
    "the device's mapping of the guest's memory was seen"
  );
  assert_eq!(
    peak.locked_kib, 0,
    "slates locks none of the guest's memory"
  );
  console
}

/// The guest's script for a destroy: one GETATTR answered, then, after `destroyed`, a second submitted and
/// kicked, and whether it was answered within five heartbeats.
fn destroy_script(
  answered_first: std::sync::mpsc::Sender<i32>,
  destroyed: std::sync::mpsc::Receiver<()>,
) -> impl FnOnce(
  std::os::fd::OwnedFd,
  std::os::fd::OwnedFd,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> {
  move |kick_write, call_read| {
    Box::pin(async move {
      let first = round_trip(&kick_write, &call_read, &getattr_message(1, 1)).await;
      let _ = answered_first.send(reply_error(&first));
      until(destroyed).await;
      common::guest::with_guest(|g| {
        g.submit(&getattr_message(2, 1), common::guest::REPLY_CAP);
      });
      let _ = rustix::io::write(&kick_write, &[1u8]);
      slates_rt::futures::sleep(slates_server::daemon::HEARTBEAT_NS * REVOKED_WATCH_HEARTBEATS)
        .await
        .unwrap();
      let answered = common::guest::with_guest(|g| g.reap()).is_some();
      drop(kick_write);
      answered
    })
  }
}

/// AUD-29-68–70 (a volume destroyed under its guest devices). Do: attach a guest device presenting the head, and
/// in a second daemon run one presenting a snapshot; let each answer one request; destroy the volume; send each
/// guest one more request. Expect: the destroy ends each device as a revocation — its terminal step sweeps its
/// references under its still-live attachment (through its view, for the snapshot) — so no later request is
/// answered, no reclaim is counted incomplete, and the device's record leaves with the volume.
#[test]
fn a_volume_destroyed_under_its_guest_devices_ends_them_cleanly() {
  for presents_snapshot in [false, true] {
    let (daemon, instance) = single_shard_daemon(if presents_snapshot {
      "virtiofs-destroy-snapshot"
    } else {
      "virtiofs-destroy-head"
    });
    let mut client = Client::connect(&instance);
    let ReplyBody::Created { id } = client.call(&scratch("destroyed")) else {
      panic!("the volume was not created");
    };
    let snapshot = presents_snapshot.then(|| {
      let ReplyBody::Snapshotted { id: snapshot, .. } =
        client.call(&RequestBody::Snapshot { volume: id })
      else {
        panic!("snapshot");
      };
      snapshot
    });
    let (first_tx, first_rx) = channel::<i32>();
    let (destroyed_tx, destroyed_rx) = channel::<()>();
    let guest = common::guest::start_guest(
      &daemon,
      id,
      Principal::Uid { uid: my_uid() },
      slates_server::virtiofs::GuestView {
        subtree: None,
        snapshot,
      },
      destroy_script(first_tx, destroyed_rx),
    );
    assert_eq!(first_rx.recv_timeout(common::guest::WAIT), Ok(0));
    assert_eq!(
      client.call(&RequestBody::Destroy { volume: id }),
      ReplyBody::Destroyed
    );
    let _ = destroyed_tx.send(());
    assert_eq!(
      guest.script.recv_timeout(common::guest::WAIT),
      Ok(false),
      "no request answered after the destroy (snapshot: {presents_snapshot})"
    );
    let outcome = guest.end.recv_timeout(common::guest::WAIT);
    let Ok(GuestDeviceOutcome::Ended(end)) = &outcome else {
      panic!("the device did not end: {outcome:?}");
    };
    assert_eq!(end.why, EndReason::Revoked, "snapshot: {presents_snapshot}");
    assert!(
      end
        .reclaimed
        .as_ref()
        .is_ok_and(|r| r.references_swept && r.sweep_refused.is_none()),
      "{end:?}"
    );
    let refusals = daemon.refusals_on_every_shard().unwrap();
    assert_eq!(
      refusals.get("virtiofs.reclaim_incomplete"),
      None,
      "{refusals:?}"
    );
    assert!(
      the_volume_goes(&mut client, id),
      "the deferred destroy completed once the device ended"
    );
    drop(client);
    drop(daemon);
  }
}

/// Whether `volume`'s destroy completes within the test's wait: `status` then refuses it.
fn the_volume_goes(client: &mut Client, volume: slates_ipc::protocol::VolumeId) -> bool {
  let deadline = Instant::now() + common::guest::WAIT;
  while Instant::now() < deadline {
    if matches!(
      client.call(&RequestBody::Status { volume }),
      ReplyBody::Refused { .. }
    ) {
      return true;
    }
    // The test thread polls the daemon over the ring; no runtime wheel serves this thread (D-9 allows a test).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(20));
  }
  false
}

/// Shape: the workload guest's RAM: rustc's working set for a one-file crate with room to spare (the mount
/// guest's 256 MiB is measured in AUD-29-77 and stays the residency figure).
#[cfg(target_os = "linux")]
const WORKLOAD_GUEST_RAM: &str = "1G";

/// Shape: how long the workload guest may run — the boot, then each roster tool twice (nine on Linux, a cargo
/// build of a one-file crate among them) under software emulation. Measured at 364 s and 317 s on Docker
/// Desktop's Apple Silicon VM (2026-10-01), so about four times that, still failing a hung guest in bounded time.
#[cfg(target_os = "linux")]
const WORKLOAD_RUN: Duration = Duration::from_secs(1500);

/// Format: the guest's script, run inside the host container's root (`/` over 9p, read-only) with the slates tag
/// at `/mnt` and a RAM `/tmp`. For each roster tool the guest holds, the workload runs on the guest's RAM and on
/// the slates mount under one fixed environment, and each run's output and resulting tree are printed between
/// markers: kind, permission bits, size, SHA-256 (or link target) and path per entry.
#[cfg(target_os = "linux")]
const GUEST_WORKLOADS: &str = r#"
export PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export HOME=/tmp/home CARGO_HOME=/tmp/cargo-home RUSTUP_HOME=/usr/local/rustup
export GIT_AUTHOR_NAME=slates GIT_AUTHOR_EMAIL=slates@example.invalid
export GIT_COMMITTER_NAME=slates GIT_COMMITTER_EMAIL=slates@example.invalid
export GIT_AUTHOR_DATE=2026-09-14T00:00:00Z GIT_COMMITTER_DATE=2026-09-14T00:00:00Z
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 TZ=UTC LC_ALL=C
mkdir -p "$HOME" "$CARGO_HOME"
while read -r name tool; do
  if ! command -v "$tool" >/dev/null 2>&1; then echo "=== SKIP $name $tool"; continue; fi
  for side in ram mount; do
    if [ "$side" = ram ]; then dir=/tmp/w/$name; else dir=/mnt/w/$name; fi
    mkdir -p "$dir"
    ( cd "$dir" && sh -e /mnt/scripts/$name.sh ) > /tmp/out.$side 2>&1
    code=$?
    echo "=== RUN $name $side $code $dir"
    cat /tmp/out.$side
    echo "=== MANIFEST"
    ( cd "$dir" && find . -mindepth 1 | LC_ALL=C sort | while IFS= read -r p; do
        kind=$(stat -c %F "$p"); mode=$(stat -c %a "$p")
        case "$kind" in
          "regular file"|"regular empty file")
            printf 'f\t%s\t%s\t%s\t%s\n' "$mode" "$(stat -c %s "$p")" "$(sha256sum < "$p" | cut -d' ' -f1)" "${p#./}" ;;
          directory) printf 'd\t%s\t0\t-\t%s\n' "$mode" "${p#./}" ;;
          "symbolic link") printf 'l\t0\t0\t%s\t%s\n' "$(readlink "$p")" "${p#./}" ;;
          *) printf 'o\t%s\t0\t-\t%s\n' "$mode" "${p#./}" ;;
        esac
      done )
    echo "=== END"
  done
done < /mnt/roster
"#;

/// The roster workloads a Linux guest runs: the whole roster but the macOS watcher (`fswatch`).
#[cfg(target_os = "linux")]
fn guest_roster() -> Vec<&'static slates_conformance::workload::Workload> {
  slates_conformance::workload::ROSTER
    .iter()
    .filter(|workload| workload.tool != "fswatch")
    .collect()
}

/// Writes `bytes` as `name` in directory `dir` over NFS.
#[cfg(target_os = "linux")]
fn put(stream: &mut TcpStream, dir: &[u8], name: &str, bytes: &[u8], xid: u32) {
  let file = common::nfs::create(stream, dir, name, xid);
  common::nfs::write(stream, &file, bytes, xid.wrapping_add(1));
}

/// Places the guest's script, the roster and each workload's script in the volume behind `root`.
#[cfg(target_os = "linux")]
fn place_workloads(stream: &mut TcpStream, root: &[u8]) {
  // The roster's bounds, the very values every other leg runs it under.
  let script = format!(
    "export {}={} {}={}\n{GUEST_WORKLOADS}",
    slates_conformance::workload::ENV_SQLITE_BUSY_MS,
    slates_conformance::workload::SQLITE_BUSY_MS,
    slates_conformance::workload::ENV_WATCH_SECONDS,
    slates_conformance::workload::WATCH_SECONDS,
  );
  put(stream, root, "run.sh", script.as_bytes(), 10);
  let roster: String = guest_roster()
    .iter()
    .map(|workload| format!("{} {}\n", workload.name, workload.tool))
    .collect();
  put(stream, root, "roster", roster.as_bytes(), 12);
  let scripts = common::nfs::mkdir(stream, root, "scripts", 14);
  for (position, workload) in guest_roster().iter().enumerate() {
    let xid = 20 + 2 * u32::try_from(position).unwrap();
    put(
      stream,
      &scripts,
      &format!("{}.sh", workload.name),
      workload.script.as_bytes(),
      xid,
    );
  }
}

/// The runs the guest printed, by workload name and side (`ram`, `mount`), and the tools it lacked.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct GuestRuns {
  runs: std::collections::BTreeMap<(String, String), slates_conformance::workload::Run>,
  skipped: Vec<String>,
}

/// One manifest line the guest printed: kind, permission bits (octal), size, digest or link target, path.
#[cfg(target_os = "linux")]
fn manifest_entry(line: &str) -> Option<slates_conformance::workload::Entry> {
  use slates_conformance::workload::{Entry, EntryKind};
  let mut fields = line.splitn(5, '\t');
  let (kind, mode, size, digest, path) = (
    fields.next()?,
    fields.next()?,
    fields.next()?,
    fields.next()?,
    fields.next()?,
  );
  let (kind, digest) = match kind {
    "f" => (EntryKind::File, digest.to_owned()),
    "d" => (EntryKind::Directory, String::new()),
    "l" => (
      EntryKind::Symlink {
        target: digest.to_owned(),
      },
      String::new(),
    ),
    _ => (EntryKind::Other, String::new()),
  };
  Some(Entry {
    path: path.to_owned(),
    kind,
    mode: u32::from_str_radix(mode, 8).ok()?,
    size: size.parse().ok()?,
    digest,
  })
}

/// The console parser reads a marker the firmware's own output shares a line with. Do: parse a console whose
/// first `=== RUN` line is exactly what the x86 serial console delivered on CI (2026-10-02, where git's RAM run went
/// missing): the firmware's `Booting from ROM..`, its reset and clear-screen sequences (`ESC c`, `ESC [ ? 7 l`,
/// `ESC [ 2 J`, `ESC [ 0 m`), a bare carriage return, then the marker; and a marker line ending in `\r` as serial lines
/// do. Expect: the run is read, with its manifest, as a clean line would be — a carriage return followed by text
/// returns to column 0, as on the terminal, and a trailing one ends the line.
#[cfg(target_os = "linux")]
#[test]
fn a_console_marker_behind_terminal_controls_is_still_read() {
  let console = "Booting from ROM..\u{1b}c\u{1b}[?7l\u{1b}[2J\u{1b}[0m.\r=== RUN git ram 0 /tmp/w/git\r\nout\r\n=== MANIFEST\r\nd\t755\t0\t-\t.git\r\n=== END\r\n";
  let parsed = guest_runs(console);
  let run = parsed
    .runs
    .get(&("git".to_owned(), "ram".to_owned()))
    .expect("the run behind the controls is read");
  assert_eq!(run.exit_code, 0);
  assert_eq!(run.directory, "/tmp/w/git");
  assert_eq!(run.manifest.entries.len(), 1);
}

/// `line` as a terminal would leave it: without the control sequences a serial console interleaves (CSI
/// `ESC [ … final`, a two-byte `ESC x`, and other C0 controls but the tab the manifest separates fields with), and
/// with a carriage return followed by more text returning to column 0, so what came before it is overwritten. The
/// firmware's `Booting from ROM..` shared the first marker's line behind a bare `\r` on CI's x86 console
/// (2026-10-02); a trailing `\r`, as serial lines end, leaves the line as it is.
#[cfg(target_os = "linux")]
fn without_terminal_controls(line: &str) -> String {
  /// Format: the escape character that opens a terminal control sequence (ECMA-48).
  const ESCAPE: char = '\u{1b}';
  /// Format: the byte after `ESC` that opens a control sequence (CSI) running to a final byte in `@..=~`.
  const CSI: char = '[';
  let mut out = String::with_capacity(line.len());
  let mut chars = line.chars();
  while let Some(c) = chars.next() {
    if c == ESCAPE {
      if chars.next() == Some(CSI) {
        for next in chars.by_ref() {
          if ('@'..='~').contains(&next) {
            break;
          }
        }
      }
    } else if c == '\r' {
      if chars.clone().next().is_some() {
        out.clear();
      }
    } else if c == '\t' || !c.is_control() {
      out.push(c);
    }
  }
  out
}

/// Parses the guest's console into its runs.
#[cfg(target_os = "linux")]
fn guest_runs(console: &str) -> GuestRuns {
  let mut parsed = GuestRuns::default();
  let cleaned: Vec<String> = console.lines().map(without_terminal_controls).collect();
  let mut lines = cleaned.iter().map(String::as_str);
  while let Some(line) = lines.next() {
    if let Some(rest) = line.strip_prefix("=== SKIP ") {
      parsed.skipped.push(rest.to_owned());
      continue;
    }
    let Some(rest) = line.strip_prefix("=== RUN ") else {
      continue;
    };
    let mut words = rest.splitn(4, ' ');
    let (Some(name), Some(side), Some(code), Some(directory)) =
      (words.next(), words.next(), words.next(), words.next())
    else {
      continue;
    };
    let mut output = String::new();
    for line in lines.by_ref() {
      if line == "=== MANIFEST" {
        break;
      }
      output.push_str(line);
      output.push('\n');
    }
    let mut entries = Vec::new();
    for line in lines.by_ref() {
      if line == "=== END" {
        break;
      }
      entries.extend(manifest_entry(line));
    }
    parsed.runs.insert(
      (name.to_owned(), side.to_owned()),
      slates_conformance::workload::Run {
        directory: directory.to_owned(),
        exit_code: code.parse().unwrap_or(-1),
        output,
        manifest: slates_conformance::workload::Manifest { entries },
      },
    );
  }
  parsed
}

/// AC-9.7 / AUD-29-68 (§6's workloads in a live guest). Do: provision a volume and place in it the conformance
/// roster's scripts; boot the live guest in workload mode — the host container's root over 9p, the slates tag at
/// its `/mnt`, RAM at `/tmp` — so for each roster tool it holds, the workload runs on the guest's RAM and on the
/// slates mount; judge each pair by the harness's own rule (`slates_conformance::workload::compare`: exit code,
/// output with the directory normalized, tree manifest under the roster's reviewed exclusions). Expect: every
/// workload the guest ran is `Identical`; the tools it lacks are named as skipped. Gated on the live guest's
/// environment; skips loudly elsewhere.
#[cfg(target_os = "linux")]
#[test]
fn a_live_guest_runs_the_roster_workloads_identically_on_slates_and_on_its_ram() {
  let Some((qemu, kernel, initrd)) = live_guest() else {
    return;
  };
  let (daemon, instance) = single_shard_daemon("virtiofs-workloads");
  let mut client = Client::connect(&instance);
  let ReplyBody::Created { id } = client.call(&RequestBody::Create {
    name: "workloads".to_owned(),
    size: SizeClass::Bounded { limit: 256 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }) else {
    panic!("the volume was not created");
  };
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon.mount_capability("workloads").unwrap().unwrap();
  let root = mount(&mut stream, &capability, 1);
  place_workloads(&mut stream, &root);
  let (ours, theirs) = rustix::net::socketpair(
    rustix::net::AddressFamily::UNIX,
    rustix::net::SocketType::STREAM,
    rustix::net::SocketFlags::CLOEXEC,
    None,
  )
  .unwrap();
  let boot_ns = u64::try_from(WORKLOAD_RUN.as_nanos()).unwrap();
  let _ended = attach_vhost(&daemon, id, (ours, boot_ns));
  let started = Instant::now();
  let console = run_qemu_with(
    &(qemu, kernel, initrd),
    &theirs,
    (WORKLOAD_RUN, " slates.workloads", true),
    &mut |_| {},
  );
  eprintln!("the workload guest ran {:?}", started.elapsed());
  drop(theirs);
  assert!(console.contains("SLATES-WORKLOADS-DONE"), "{console}");
  let parsed = guest_runs(&console);
  eprintln!("skipped in the guest: {:?}", parsed.skipped);
  let mut judged = 0;
  for workload in guest_roster() {
    let ram = parsed
      .runs
      .get(&(workload.name.to_owned(), "ram".to_owned()));
    let slates = parsed
      .runs
      .get(&(workload.name.to_owned(), "mount".to_owned()));
    let (Some(ram), Some(slates)) = (ram, slates) else {
      continue;
    };
    let status = slates_conformance::workload::compare(workload, ram, slates);
    eprintln!("guest workload {}: {status:?}", workload.name);
    assert_eq!(
      status,
      slates_conformance::record::WorkloadStatus::Identical,
      "{}: ram {ram:?}\nslates {slates:?}",
      workload.name
    );
    judged += 1;
  }
  assert!(judged > 0, "no workload ran in the guest: {console}");
  // A roster workload the guest neither reported skipped nor ran on both sides is a failure, never a silent
  // pass (CI 2026-10-02: git vanished from a passing run). The console lines naming each such workload are
  // the evidence of where its run went.
  let missing: Vec<&str> = guest_roster()
    .iter()
    .map(|workload| workload.name)
    .filter(|name| {
      !parsed
        .skipped
        .iter()
        .any(|skipped| skipped.split(' ').next() == Some(*name))
        && !(parsed
          .runs
          .contains_key(&((*name).to_owned(), "ram".to_owned()))
          && parsed
            .runs
            .contains_key(&((*name).to_owned(), "mount".to_owned())))
    })
    .collect();
  let evidence: Vec<&str> = console
    .lines()
    .filter(|line| missing.iter().any(|name| line.contains(name)))
    .take(GUEST_EVIDENCE_LINES)
    .collect();
  assert!(
    missing.is_empty(),
    "workloads neither skipped nor run on both sides: {missing:?}; console lines naming them:\n{}",
    evidence.join("\n")
  );
}

/// Shape: the console lines kept as evidence for a workload that went missing in the guest.
#[cfg(target_os = "linux")]
const GUEST_EVIDENCE_LINES: usize = 60;
