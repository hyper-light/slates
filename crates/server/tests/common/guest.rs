//! A simulated guest on a daemon's guest device (§4.6 A-9, D-2, RQ-20): a minimal guest driver over real
//! virtqueues in a simulated guest memory, an in-process VMM seam whose doorbell and notification are pipes,
//! and the FUSE requests a guest kernel sends. The guest and the device share the owning shard's thread, as an
//! in-process VMM's parties share one address space: never concurrent.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use slates_bridge_fuse::abi::IN_HEADER_LEN;
use slates_bridge_virtiofs::admission::{Doorbell, Drained, SeamError, VmmSeam};
use slates_bridge_virtiofs::device::{DeviceConfig, FsTag};
use slates_bridge_virtiofs::memory::{GuestAddr, GuestMemory, GuestMemoryError, GuestRange};
use slates_bridge_virtiofs::sim::SimGuestMemory;
use slates_bridge_virtiofs::virtqueue::{
  DESCRIPTOR_LEN, QueueLayout, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
};
use slates_db::catalog::Principal;
use slates_ipc::protocol::VolumeId;
use slates_rt::readiness::readable;
use slates_server::Daemon;
use slates_server::virtiofs::{GuestDeviceOutcome, GuestView};

/// Shape: how long the test waits for the guest and the device loop to report.
pub(crate) const WAIT: Duration = Duration::from_secs(20);
/// Shape: the simulated guest's memory (1 MiB), its ring areas and where its buffers start.
pub(crate) const GUEST_RAM: u64 = 1 << 20;
const RING_BASE: u64 = 0x1000;
const RING_STRIDE: u64 = 0x10000;
const BUFFER_BASE: u64 = 0x80000;
/// Shape: the queue sizes (hiprio, request) and the reply room posted per request.
const QUEUE_SIZES: [u16; 2] = [8, 32];
pub(crate) const REPLY_CAP: u32 = 512;
/// Shape: bytes one drain read takes.
const DRAIN: usize = 64;
/// Format: `O_RDWR`.
const O_RDWR: u32 = 2;

// --- A minimal simulated guest driver: one readable and one writable descriptor per request. ---

/// The ring layout for a queue of `size` with its descriptor table at `desc` (§2.7 alignments).
pub(crate) fn layout_at(desc: u64, size: u16) -> QueueLayout {
  let avail = desc + u64::from(size) * DESCRIPTOR_LEN;
  let used = (avail + 6 + 2 * u64::from(size)).next_multiple_of(4);
  QueueLayout {
    size,
    descriptor_table: GuestAddr(desc),
    available_ring: GuestAddr(avail),
    used_ring: GuestAddr(used),
  }
}

/// The driver, over any guest memory: the simulated arena by default, or a real shared mapping (the vhost-user
/// front end's memory object, mapped a second time).
pub(crate) struct Guest<M: GuestMemory = SimGuestMemory> {
  memory: M,
  layouts: Vec<QueueLayout>,
  next_buffer: u64,
  avail_idx: u16,
  used_seen: u16,
  next_desc: u16,
  writable: BTreeMap<u16, GuestRange>,
}

impl Guest {
  pub(crate) fn new() -> Guest {
    Guest::over(SimGuestMemory::new(GUEST_RAM))
  }
}

impl<M: GuestMemory> Guest<M> {
  /// The driver over `memory`, which must hold [`GUEST_RAM`] bytes from guest address zero.
  pub(crate) fn over(memory: M) -> Guest<M> {
    Guest {
      memory,
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

  /// Every queue's layout, in queue order (high priority, then the request queue).
  pub(crate) fn layouts(&self) -> &[QueueLayout] {
    &self.layouts
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
  pub(crate) fn submit(&mut self, request: &[u8], reply_capacity: u32) -> u16 {
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
    // The driver's barrier before publishing the index (§2.7.13.3): the chain is visible first.
    self
      .memory
      .order(slates_bridge_virtiofs::memory::Edge::Release);
    self.write(layout.available_ring.0 + 2, &idx.to_le_bytes());
    head
  }

  /// The next used element of the request queue: `(head, len)`.
  pub(crate) fn reap(&mut self) -> Option<(u16, u32)> {
    let layout = self.layouts[1];
    let idx = u16::from_le_bytes(self.read(layout.used_ring.0 + 2, 2).try_into().unwrap());
    if idx == self.used_seen {
      return None;
    }
    // The element and the reply the index publishes are read after it (§2.7.14).
    self
      .memory
      .order(slates_bridge_virtiofs::memory::Edge::Acquire);
    let slot = u64::from(self.used_seen % layout.size);
    let at = layout.used_ring.0 + 4 + slot * 8;
    let id = u32::from_le_bytes(self.read(at, 4).try_into().unwrap());
    let len = u32::from_le_bytes(self.read(at + 4, 4).try_into().unwrap());
    self.used_seen = self.used_seen.wrapping_add(1);
    Some((u16::try_from(id).unwrap(), len))
  }

  pub(crate) fn reply_of(&self, head: u16, len: u32) -> Vec<u8> {
    let range = self.writable[&head];
    self.read(range.start().0, usize::try_from(len).unwrap())
  }
}

thread_local! {
  /// The guest, shared by the device task and the guest task on the owning shard (the in-process
  /// VMM's shape); created by whichever side touches it first.
  static GUEST: RefCell<Option<Guest>> = const { RefCell::new(None) };
}

pub(crate) fn with_guest<R>(f: impl FnOnce(&mut Guest) -> R) -> R {
  GUEST.with(|guest| f(guest.borrow_mut().get_or_insert_with(Guest::new)))
}

pub(crate) struct SharedGuestMemory;

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
  fn order(&self, edge: slates_bridge_virtiofs::memory::Edge) {
    with_guest(|g| g.memory.order(edge));
  }
}

/// The harness's seam over two pipes; `consumer` is the principal the harness enrolled the guest as.
pub(crate) struct PipeVmm {
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

pub(crate) fn drain(fd: &OwnedFd) -> Result<Drained, SeamError> {
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

pub(crate) fn pipe() -> (OwnedFd, OwnedFd) {
  let (read, write) = rustix::pipe::pipe().unwrap();
  rustix::io::ioctl_fionbio(&read, true).unwrap();
  (read, write)
}

// --- The FUSE requests a guest kernel sends. ---

pub(crate) fn message(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut m = vec![0u8; total];
  m[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  m[4..8].copy_from_slice(&opcode.to_le_bytes());
  m[8..16].copy_from_slice(&unique.to_le_bytes());
  m[16..24].copy_from_slice(&nodeid.to_le_bytes());
  m[IN_HEADER_LEN..].copy_from_slice(body);
  m
}

pub(crate) fn reply_error(reply: &[u8]) -> i32 {
  i32::from_le_bytes(reply[4..8].try_into().unwrap())
}

pub(crate) fn u64_at(bytes: &[u8], at: usize) -> u64 {
  u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// `fuse_create_in` (flags, mode, umask, open_flags) then the name.
pub(crate) fn create_body(name: &str) -> Vec<u8> {
  let mut b = Vec::new();
  b.extend_from_slice(&O_RDWR.to_le_bytes());
  b.extend_from_slice(&0o644u32.to_le_bytes());
  b.extend_from_slice(&[0u8; 8]);
  b.extend_from_slice(name.as_bytes());
  b.push(0);
  b
}

/// `fuse_write_in` (fh, offset, size, then five words slates skips) then the data.
pub(crate) fn write_body(fh: u64, data: &[u8]) -> Vec<u8> {
  let mut b = Vec::new();
  b.extend_from_slice(&fh.to_le_bytes());
  b.extend_from_slice(&0u64.to_le_bytes());
  b.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
  b.extend_from_slice(&[0u8; 20]);
  b.extend_from_slice(data);
  b
}

/// `fuse_flush_in`: fh, two words slates skips, then the lock owner — what a guest's `close` sends.
pub(crate) fn flush_body(fh: u64) -> Vec<u8> {
  let mut b = fh.to_le_bytes().to_vec();
  b.extend_from_slice(&[0u8; 16]);
  b
}

/// `fuse_release_in`: fh then three words slates skips.
pub(crate) fn release_body(fh: u64) -> Vec<u8> {
  let mut b = fh.to_le_bytes().to_vec();
  b.extend_from_slice(&[0u8; 16]);
  b
}

/// The guest submits one request, kicks, awaits the notification, and returns the reply.
pub(crate) async fn round_trip(
  kick_write: &OwnedFd,
  call_read: &OwnedFd,
  request: &[u8],
) -> Vec<u8> {
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
pub(crate) fn my_uid() -> u32 {
  rustix::process::getuid().as_raw()
}

/// A guest started on a daemon's device and still running: its script's result and the device loop's outcome,
/// each delivered once.
pub(crate) struct StartedGuest<R> {
  /// The script's result, once it finished.
  pub(crate) script: Receiver<R>,
  /// The device loop's outcome, once it ended.
  pub(crate) end: Receiver<GuestDeviceOutcome>,
  /// The device's attachment record id, once it was admitted and recorded.
  pub(crate) admitted: Receiver<u64>,
}

/// Attaches a guest device as `consumer` to `volume` and starts `script` as the guest on the owning shard,
/// returning at once, so the test thread can act on the fleet while the guest runs.
pub(crate) fn start_guest<R: Send + 'static>(
  daemon: &Daemon,
  volume: VolumeId,
  consumer: Principal,
  view: GuestView,
  script: impl FnOnce(
    OwnedFd,
    OwnedFd,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send>>
  + Send
  + 'static,
) -> StartedGuest<R> {
  let (kick_read, kick_write) = pipe();
  let (call_read, call_write) = pipe();
  let (end_tx, end_rx) = channel::<GuestDeviceOutcome>();
  let (admitted_tx, admitted_rx) = channel::<u64>();
  let (script_tx, script_rx) = channel::<R>();
  daemon
    .attach_guest_device(
      volume,
      FsTag::new("slates").unwrap(),
      view,
      PipeVmm {
        kick_read,
        call_write,
        consumer,
        memory: SharedGuestMemory,
      },
      slates_server::virtiofs::GuestHarness {
        on_admitted: Box::new(move |attachment| {
          let _ = admitted_tx.send(attachment);
        }),
        on_end: Box::new(move |outcome| {
          let _ = end_tx.send(outcome);
        }),
      },
    )
    .unwrap();
  daemon
    .spawn_on_owner(volume, async move {
      let result = script(kick_write, call_read).await;
      let _ = script_tx.send(result);
    })
    .unwrap();
  StartedGuest {
    script: script_rx,
    end: end_rx,
    admitted: admitted_rx,
  }
}

/// Attaches a guest device as `consumer` to `volume` and runs `script` as the guest on the owning
/// shard; returns the device's outcome and the script's result.
pub(crate) fn run_guest<R: Send + 'static>(
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
  run_guest_viewing(daemon, volume, consumer, GuestView::default(), script)
}

/// [`run_guest`] with the device presenting `view` (a subtree, or a snapshot read-only).
pub(crate) fn run_guest_viewing<R: Send + 'static>(
  daemon: &Daemon,
  volume: VolumeId,
  consumer: Principal,
  view: GuestView,
  script: impl FnOnce(
    OwnedFd,
    OwnedFd,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send>>
  + Send
  + 'static,
) -> (GuestDeviceOutcome, R) {
  let guest = start_guest(daemon, volume, consumer, view, script);
  let result = guest
    .script
    .recv_timeout(WAIT)
    .expect("the guest script finished");
  let outcome = guest.end.recv_timeout(WAIT).expect("the device loop ended");
  (outcome, result)
}
