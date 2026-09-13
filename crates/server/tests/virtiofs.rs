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

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::reply::EntryOut;
use slates_bridge_virtiofs::admission::{Doorbell, Drained, GuestTransport, SeamError, VmmSeam};
use slates_bridge_virtiofs::device::{DeviceConfig, FsTag};
use slates_bridge_virtiofs::memory::{GuestAddr, GuestMemory, GuestMemoryError, GuestRange};
use slates_bridge_virtiofs::serve::EndReason;
use slates_bridge_virtiofs::sim::SimGuestMemory;
use slates_bridge_virtiofs::virtqueue::{
  DESCRIPTOR_LEN, QueueLayout, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
};
use slates_db::catalog::Principal;
use slates_ipc::protocol::{
  Direction, NamePolicy, ReplyBody, RequestBody, SizeClass, VolumeId, pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_rt::readiness::readable;
use slates_server::virtiofs::{GuestDeviceOutcome, guest_transport_capabilities};
use slates_server::{Daemon, DaemonConfig, SegmentSource};
use slates_wire::request::RequestId;

mod common;
use common::nfs::{lookup, mount, read};

/// Shape: the probe budget of the quick profile (milliseconds); an input to derivations, not a gate.
const PROBE_MS: u64 = 5;
/// Shape: the reply deadline (nanoseconds): five seconds, far past any served verb.
const DEADLINE_NS: u64 = 5_000_000_000;
/// Shape: how long a client waits for the daemon or a full ring before giving up.
const CREDIT_WAIT: Duration = Duration::from_secs(5);
/// Shape: how long the test waits for the guest and the device loop to report.
const WAIT: Duration = Duration::from_secs(20);
/// Shape: the simulated guest's memory (1 MiB), its ring areas and where its buffers start.
const GUEST_RAM: u64 = 1 << 20;
const RING_BASE: u64 = 0x1000;
const RING_STRIDE: u64 = 0x10000;
const BUFFER_BASE: u64 = 0x80000;
/// Shape: the queue sizes (hiprio, request) and the reply room posted per request.
const QUEUE_SIZES: [u16; 2] = [8, 32];
const REPLY_CAP: u32 = 512;
/// Shape: bytes one drain read takes.
const DRAIN: usize = 64;
/// Format: `O_RDWR`; `EPERM`.
const O_RDWR: u32 = 2;
const EPERM: i32 = 1;
/// The bytes the guest writes and the host reads back.
const PAYLOAD: &[u8] = b"written by a guest through virtqueues, read back by the host over NFS\n";

fn profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
}

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

// --- A minimal simulated guest driver: one readable and one writable descriptor per request. ---

/// The ring layout for a queue of `size` with its descriptor table at `desc` (§2.7 alignments).
fn layout_at(desc: u64, size: u16) -> QueueLayout {
  let avail = desc + u64::from(size) * DESCRIPTOR_LEN;
  let used = (avail + 6 + 2 * u64::from(size)).next_multiple_of(4);
  QueueLayout {
    size,
    descriptor_table: GuestAddr(desc),
    available_ring: GuestAddr(avail),
    used_ring: GuestAddr(used),
  }
}

struct Guest {
  memory: SimGuestMemory,
  layouts: Vec<QueueLayout>,
  next_buffer: u64,
  avail_idx: u16,
  used_seen: u16,
  next_desc: u16,
  writable: BTreeMap<u16, GuestRange>,
}

impl Guest {
  fn new() -> Guest {
    Guest {
      memory: SimGuestMemory::new(GUEST_RAM),
      layouts: QUEUE_SIZES
        .iter()
        .enumerate()
        .map(|(queue, size)| {
          layout_at(
            RING_BASE + u64::try_from(queue).unwrap() * RING_STRIDE,
            *size,
          )
        })
        .collect(),
      next_buffer: BUFFER_BASE,
      avail_idx: 0,
      used_seen: 0,
      next_desc: 0,
      writable: BTreeMap::new(),
    }
  }

  fn write(&mut self, at: u64, bytes: &[u8]) {
    let range = GuestRange::new(GuestAddr(at), u64::try_from(bytes.len()).unwrap()).unwrap();
    self.memory.write(range, bytes).unwrap();
  }

  fn read(&self, at: u64, len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    self
      .memory
      .read(
        GuestRange::new(GuestAddr(at), u64::try_from(len).unwrap()).unwrap(),
        &mut bytes,
      )
      .unwrap();
    bytes
  }

  fn descriptor(&mut self, index: u16, addr: u64, len: u32, flags: u16, next: u16) {
    let at = self.layouts[1].descriptor_table.0 + u64::from(index) * DESCRIPTOR_LEN;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&addr.to_le_bytes());
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.extend_from_slice(&next.to_le_bytes());
    self.write(at, &bytes);
  }

  /// Submits `request` on the request queue with `reply_capacity` writable bytes; returns the head.
  fn submit(&mut self, request: &[u8], reply_capacity: u32) -> u16 {
    let head = self.next_desc;
    let tail = head + 1;
    self.next_desc = (self.next_desc + 2) % QUEUE_SIZES[1];
    let request_at = self.next_buffer;
    self.next_buffer += u64::try_from(request.len()).unwrap();
    let reply_at = self.next_buffer;
    self.next_buffer += u64::from(reply_capacity);
    self.write(request_at, request);
    let request_len = u32::try_from(request.len()).unwrap();
    self.descriptor(head, request_at, request_len, VIRTQ_DESC_F_NEXT, tail);
    self.descriptor(tail, reply_at, reply_capacity, VIRTQ_DESC_F_WRITE, 0);
    self.writable.insert(
      head,
      GuestRange::new(GuestAddr(reply_at), u64::from(reply_capacity)).unwrap(),
    );
    let layout = self.layouts[1];
    let slot = u64::from(self.avail_idx % layout.size);
    self.write(layout.available_ring.0 + 4 + slot * 2, &head.to_le_bytes());
    self.avail_idx = self.avail_idx.wrapping_add(1);
    let idx = self.avail_idx;
    self.write(layout.available_ring.0 + 2, &idx.to_le_bytes());
    head
  }

  /// The next used element of the request queue: `(head, len)`.
  fn reap(&mut self) -> Option<(u16, u32)> {
    let layout = self.layouts[1];
    let idx = u16::from_le_bytes(self.read(layout.used_ring.0 + 2, 2).try_into().unwrap());
    if idx == self.used_seen {
      return None;
    }
    let slot = u64::from(self.used_seen % layout.size);
    let at = layout.used_ring.0 + 4 + slot * 8;
    let id = u32::from_le_bytes(self.read(at, 4).try_into().unwrap());
    let len = u32::from_le_bytes(self.read(at + 4, 4).try_into().unwrap());
    self.used_seen = self.used_seen.wrapping_add(1);
    Some((u16::try_from(id).unwrap(), len))
  }

  fn reply_of(&self, head: u16, len: u32) -> Vec<u8> {
    let range = self.writable[&head];
    self.read(range.start().0, usize::try_from(len).unwrap())
  }
}

thread_local! {
  /// The guest, shared by the device task and the guest task on the owning shard (the in-process
  /// VMM's shape); created by whichever side touches it first.
  static GUEST: RefCell<Option<Guest>> = const { RefCell::new(None) };
}

fn with_guest<R>(f: impl FnOnce(&mut Guest) -> R) -> R {
  GUEST.with(|guest| f(guest.borrow_mut().get_or_insert_with(Guest::new)))
}

struct SharedGuestMemory;

impl GuestMemory for SharedGuestMemory {
  fn check(&self, range: GuestRange) -> Result<(), GuestMemoryError> {
    with_guest(|g| g.memory.check(range))
  }
  fn read(&self, range: GuestRange, out: &mut [u8]) -> Result<(), GuestMemoryError> {
    with_guest(|g| g.memory.read(range, out))
  }
  fn write(&mut self, range: GuestRange, bytes: &[u8]) -> Result<(), GuestMemoryError> {
    with_guest(|g| g.memory.write(range, bytes))
  }
}

/// The harness's seam over two pipes; `consumer` is the principal the harness enrolled the guest as.
struct PipeVmm {
  kick_read: OwnedFd,
  call_write: OwnedFd,
  consumer: Principal,
  /// The zero-size handle the seam lends: every access goes through the guest's thread-local.
  memory: SharedGuestMemory,
}

impl VmmSeam for PipeVmm {
  fn consumer(&mut self) -> Result<Principal, SeamError> {
    Ok(self.consumer.clone())
  }
  fn memory(&mut self) -> Result<&mut dyn GuestMemory, SeamError> {
    Ok(&mut self.memory)
  }
  fn queues(&mut self) -> Result<Vec<QueueLayout>, SeamError> {
    Ok(with_guest(|g| g.layouts.clone()))
  }
  fn publish(&mut self, _config: &DeviceConfig) -> Result<(), SeamError> {
    Ok(())
  }
  fn notify_used(&mut self, _queue: u16) -> Result<(), SeamError> {
    rustix::io::write(&self.call_write, &[1u8])
      .map(|_| ())
      .map_err(|_| SeamError::NotifyRefused { queue: 0 })
  }
  fn doorbell(&self) -> Doorbell {
    Doorbell::Descriptor(self.kick_read.as_raw_fd())
  }
  fn drain_doorbell(&mut self) -> Result<Drained, SeamError> {
    drain(&self.kick_read)
  }
  fn release(&mut self) {}
}

fn drain(fd: &OwnedFd) -> Result<Drained, SeamError> {
  let mut kicked = false;
  let mut buffer = [0u8; DRAIN];
  loop {
    match rustix::io::read(fd, &mut buffer) {
      Ok(0) => return Ok(Drained::HungUp),
      Ok(_) => kicked = true,
      Err(rustix::io::Errno::AGAIN) => {
        return Ok(if kicked {
          Drained::Kicked
        } else {
          Drained::Nothing
        });
      }
      Err(rustix::io::Errno::INTR) => {}
      Err(_) => return Ok(Drained::HungUp),
    }
  }
}

fn pipe() -> (OwnedFd, OwnedFd) {
  let (read, write) = rustix::pipe::pipe().unwrap();
  rustix::io::ioctl_fionbio(&read, true).unwrap();
  (read, write)
}

// --- The FUSE requests a guest kernel sends. ---

fn message(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut m = vec![0u8; total];
  m[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  m[4..8].copy_from_slice(&opcode.to_le_bytes());
  m[8..16].copy_from_slice(&unique.to_le_bytes());
  m[16..24].copy_from_slice(&nodeid.to_le_bytes());
  m[IN_HEADER_LEN..].copy_from_slice(body);
  m
}

fn reply_error(reply: &[u8]) -> i32 {
  i32::from_le_bytes(reply[4..8].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
  u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// `fuse_create_in` (flags, mode, umask, open_flags) then the name.
fn create_body(name: &str) -> Vec<u8> {
  let mut b = Vec::new();
  b.extend_from_slice(&O_RDWR.to_le_bytes());
  b.extend_from_slice(&0o644u32.to_le_bytes());
  b.extend_from_slice(&[0u8; 8]);
  b.extend_from_slice(name.as_bytes());
  b.push(0);
  b
}

/// `fuse_write_in` (fh, offset, size, then five words slates skips) then the data.
fn write_body(fh: u64, data: &[u8]) -> Vec<u8> {
  let mut b = Vec::new();
  b.extend_from_slice(&fh.to_le_bytes());
  b.extend_from_slice(&0u64.to_le_bytes());
  b.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
  b.extend_from_slice(&[0u8; 20]);
  b.extend_from_slice(data);
  b
}

/// `fuse_release_in`: fh then three words slates skips.
fn release_body(fh: u64) -> Vec<u8> {
  let mut b = fh.to_le_bytes().to_vec();
  b.extend_from_slice(&[0u8; 16]);
  b
}

/// The guest submits one request, kicks, awaits the notification, and returns the reply.
async fn round_trip(kick_write: &OwnedFd, call_read: &OwnedFd, request: &[u8]) -> Vec<u8> {
  let head = with_guest(|g| g.submit(request, REPLY_CAP));
  rustix::io::write(kick_write, &[1u8]).unwrap();
  loop {
    readable(call_read.as_raw_fd()).await.unwrap();
    if drain(call_read).unwrap() == Drained::Kicked {
      break;
    }
  }
  let (id, len) = with_guest(|g| g.reap()).expect("a used element after the notification");
  assert_eq!(id, head);
  with_guest(|g| g.reply_of(head, len))
}

/// The uid this test runs as: the principal the daemon's rendezvous established for the client,
/// hence the volume's owner.
fn my_uid() -> u32 {
  rustix::process::getuid().as_raw()
}

/// Attaches a guest device as `consumer` to `volume` and runs `script` as the guest on the owning
/// shard; returns the device's outcome and the script's result.
fn run_guest<R: Send + 'static>(
  daemon: &Daemon,
  volume: VolumeId,
  consumer: Principal,
  script: impl FnOnce(
    OwnedFd,
    OwnedFd,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send>>
  + Send
  + 'static,
) -> (GuestDeviceOutcome, R) {
  let (kick_read, kick_write) = pipe();
  let (call_read, call_write) = pipe();
  let (end_tx, end_rx) = channel::<GuestDeviceOutcome>();
  let (script_tx, script_rx) = channel::<R>();
  daemon
    .attach_guest_device(
      volume,
      FsTag::new("slates").unwrap(),
      PipeVmm {
        kick_read,
        call_write,
        consumer,
        memory: SharedGuestMemory,
      },
      Box::new(move |outcome| {
        let _ = end_tx.send(outcome);
      }),
    )
    .unwrap();
  daemon
    .spawn_on_owner(volume, async move {
      let result = script(kick_write, call_read).await;
      let _ = script_tx.send(result);
    })
    .unwrap();
  let result = script_rx
    .recv_timeout(WAIT)
    .expect("the guest script finished");
  let outcome = end_rx.recv_timeout(WAIT).expect("the device loop ended");
  (outcome, result)
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
  let root = mount(&mut stream, "/guest", 1);
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
