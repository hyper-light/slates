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
  u64_at, write_body,
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
  let refusals = first.fleet_refusals().unwrap();
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
