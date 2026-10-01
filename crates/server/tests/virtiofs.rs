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
      Box::new(move |outcome| {
        let _ = end_tx.send(outcome);
      }),
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
  (qemu, kernel, initrd): &(String, String, String),
  socket: &std::os::fd::OwnedFd,
) -> String {
  use std::os::fd::AsRawFd;
  // The child inherits the descriptor at its own number (QEMU's `fd=` option names it).
  rustix::io::fcntl_setfd(socket, rustix::io::FdFlags::empty()).unwrap();
  let chardev = format!("socket,id=vfs,fd={}", socket.as_raw_fd());
  let mut child = std::process::Command::new(qemu)
    .args([
      "-machine",
      "virt,memory-backend=mem",
      "-cpu",
      "max",
      "-smp",
      "1",
      "-m",
      "256M",
      "-object",
      "memory-backend-memfd,id=mem,size=256M,share=on",
      "-nographic",
      "-nic",
      "none",
      "-no-reboot",
      "-kernel",
      kernel,
      "-initrd",
      initrd,
      "-append",
      "console=ttyAMA0 rdinit=/init panic=-1 quiet",
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
    if started.elapsed() > GUEST_RUN {
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
    format!("(the guest did not power off within {GUEST_RUN:?})\n{console}")
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
  let console = run_qemu(&guest, &theirs);
  drop(theirs);
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
