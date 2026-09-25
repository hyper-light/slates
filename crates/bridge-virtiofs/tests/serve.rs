//! The device loop on the real runtime (§4.3, §4.6 A-9, D-9; the runtime leg of AC-4.11/T-4.13):
//! an admitted device is served by a perpetual task on a shard of `slates-rt`, woken through the
//! shard's driver by a real doorbell descriptor (a pipe here; an eventfd from a VMM), and a
//! simulated guest driver — another task on the same shard, sharing the guest memory the way an
//! in-process VMM does — kicks it, awaits the used-buffer notification on a second pipe, and reads
//! the replies back. Closing the kick ends the loop through the terminal step; so does a revoke
//! request from the shard; an in-process seam with no doorbell ends it at once.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// The loop rides the runtime's Unix descriptor readiness.
#![cfg(unix)]

mod common;

use std::cell::RefCell;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::mpsc::channel;
use std::time::Duration;

use common::{SimDriver, SimVmm, message, reply_error, store, vid, volume};
use slates_bridge_core::{Attachments, Bridge, Rights, VolumeBridge};
use slates_bridge_fuse::abi::Opcode;
use slates_bridge_virtiofs::admission::{
  AdmittedDevice, Doorbell, Drained, GuestAttachRequest, GuestTransport, SeamError, VmmSeam, admit,
};
use slates_bridge_virtiofs::credit::AttachmentCredits;
use slates_bridge_virtiofs::device::{DeviceConfig, FIRST_REQUEST_QUEUE, FsTag};
use slates_bridge_virtiofs::memory::{GuestMemory, GuestMemoryError, GuestRange};
use slates_bridge_virtiofs::serve::{
  BridgeAccess, EndReason, ServeEnd, register, request_revoke, serve_loop,
};
use slates_bridge_virtiofs::virtqueue::QueueLayout;
use slates_db::catalog::Principal;
use slates_machine::derived;
use slates_rt::futures;
use slates_rt::readiness::readable;
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_vfs::error::VfsError;
use slates_vfs::volume::{Store, Volume};

/// Shape: the runtime configuration the runtime's own tests use (one shard, small arenas).
fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// Shape: how long the test waits for the tasks to report before it fails.
const WAIT: Duration = Duration::from_secs(10);
/// Shape: the reply room posted per request.
const REPLY_CAP: u32 = 256;
/// Shape: the queue sizes.
const QUEUE_SIZES: [u16; 2] = [8, 32];
/// Shape: the device loops one shard may run in these tests.
const DEVICE_BOUND: usize = 4;
/// Shape: bytes one drain read takes: more than the kicks a test sends between waits.
const DRAIN: usize = 64;

thread_local! {
  /// The guest, shared by the device task and the guest task on the one shard — the in-process
  /// VMM's shape: one address space, two parties, never concurrent.
  static GUEST: RefCell<Option<SimDriver>> = const { RefCell::new(None) };
}

/// The guest memory as the seam lends it: every access borrows the shared guest briefly.
struct SharedGuestMemory;

fn with_guest<R>(f: impl FnOnce(&mut SimDriver) -> R) -> R {
  GUEST.with(|guest| f(guest.borrow_mut().as_mut().expect("the guest exists")))
}

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

/// A VMM seam over two pipes: the kick (its read end is the doorbell) and the call (its write end
/// raises the used-buffer notification).
struct PipeVmm {
  kick_read: OwnedFd,
  call_write: OwnedFd,
  memory: SharedGuestMemory,
  notified: u64,
}

impl std::fmt::Debug for PipeVmm {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("PipeVmm")
      .field("notified", &self.notified)
      .finish()
  }
}

impl VmmSeam for PipeVmm {
  fn consumer(&mut self) -> Result<Principal, SeamError> {
    Ok(Principal::Uid { uid: 501 })
  }
  fn memory(&mut self) -> Result<&mut dyn GuestMemory, SeamError> {
    Ok(&mut self.memory)
  }
  fn queues(&mut self) -> Result<Vec<QueueLayout>, SeamError> {
    Ok(with_guest(|g| g.layouts()))
  }
  fn publish(&mut self, _config: &DeviceConfig) -> Result<(), SeamError> {
    Ok(())
  }
  fn notify_used(&mut self, _queue: u16) -> Result<(), SeamError> {
    self.notified += 1;
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

/// Drains a non-blocking pipe: kicked, nothing, or hung up.
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

/// A non-blocking pipe pair.
fn pipe() -> (OwnedFd, OwnedFd) {
  let (read, write) = rustix::pipe::pipe().unwrap();
  rustix::io::ioctl_fionbio(&read, true).unwrap();
  (read, write)
}

/// The test's owned volume, lent to the loop as its bridge.
struct OwnedVolume {
  store: Store,
  volume: Volume,
  /// The owner's attachment registry the device is admitted into (GAP-A9-4).
  registry: Attachments,
}

impl OwnedVolume {
  fn fresh() -> OwnedVolume {
    let mut store = store();
    let volume = volume(&mut store);
    OwnedVolume {
      store,
      volume,
      registry: Attachments::new(),
    }
  }
}

impl BridgeAccess for OwnedVolume {
  fn with_bridge<R>(
    &mut self,
    f: impl FnOnce(&mut dyn Bridge, &mut Attachments) -> R,
  ) -> Result<R, VfsError> {
    let mut bridge = VolumeBridge::new(vid(), &mut self.volume, &mut self.store);
    Ok(f(&mut bridge, &mut self.registry))
  }
}

fn credits() -> AttachmentCredits {
  AttachmentCredits {
    requests: derived!(8, "the test's request credit", ["test"]),
    bytes: derived!(1 << 20, "the test's byte credit", ["test"]),
  }
}

fn request() -> GuestAttachRequest {
  GuestAttachRequest {
    transport: GuestTransport::InProcess,
    volume: vid(),
    dax: false,
    notification_queue: false,
  }
}

/// The access list of these tests: every authenticated consumer may read and write.
fn rw(_consumer: &Principal) -> Rights {
  Rights {
    read: true,
    write: true,
  }
}

/// Builds the guest and admits a device over a pipe seam; returns the admitted device and the
/// guest's ends (the kick's write end, the call's read end).
fn admitted_over_pipes(registry: &mut Attachments) -> (AdmittedDevice<PipeVmm>, OwnedFd, OwnedFd) {
  let (kick_read, kick_write) = pipe();
  let (call_read, call_write) = pipe();
  GUEST.with(|guest| *guest.borrow_mut() = Some(SimDriver::new(&QUEUE_SIZES)));
  let seam = PipeVmm {
    kick_read,
    call_write,
    memory: SharedGuestMemory,
    notified: 0,
  };
  let admitted = admit(
    request(),
    seam,
    DeviceConfig::new(FsTag::new("slates").unwrap()),
    credits(),
    rw,
    registry,
  )
  .unwrap();
  (admitted, kick_write, call_read)
}

/// The guest submits one GETATTR of the root, kicks, awaits the notification, and returns the
/// reply's errno.
async fn guest_round_trip(unique: u64, kick_write: &OwnedFd, call_read: &OwnedFd) -> i32 {
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  let head = with_guest(|g| {
    g.submit(
      rq,
      &message(Opcode::GetAttr.to_wire(), unique, 1, &[0u8; 16]),
      REPLY_CAP,
      1,
    )
  });
  rustix::io::write(kick_write, &[1u8]).unwrap();
  loop {
    readable(call_read.as_raw_fd()).await.unwrap();
    if drain(call_read).unwrap() == Drained::Kicked {
      break;
    }
  }
  let (id, len) = with_guest(|g| g.reap(rq)).expect("a used element after the notification");
  assert_eq!(id, head);
  with_guest(|g| reply_error(&g.reply_of(rq, head, len)))
}

/// The loop serves three kicks through the driver — each answered and notified — and, when the
/// guest closes the kick, ends through the terminal step with the hangup named; the wake and
/// pass counters moved.
#[test]
fn the_loop_serves_kicks_through_the_driver_and_ends_on_hangup() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let (end_tx, end_rx) = channel::<ServeEnd>();
  let (guest_tx, guest_rx) = channel::<(Vec<i32>, u64)>();
  rt.spawn_on(shard, async move {
    let mut owned = OwnedVolume::fresh();
    let (admitted, kick_write, call_read) = admitted_over_pipes(&mut owned.registry);
    let id = register(DEVICE_BOUND).unwrap();
    let device_task = futures::spawn(async move {
      let end = serve_loop(id, admitted, owned).await;
      let _ = end_tx.send(end);
    })
    .unwrap();
    let _ = futures::detach(device_task);
    let guest_task = futures::spawn(async move {
      let mut errors = Vec::new();
      for unique in 1..=3 {
        errors.push(guest_round_trip(unique, &kick_write, &call_read).await);
      }
      drop(kick_write);
      let _ = guest_tx.send((errors, 3));
    })
    .unwrap();
    let _ = futures::detach(guest_task);
  })
  .unwrap();

  let (errors, sent) = guest_rx.recv_timeout(WAIT).expect("the guest finished");
  assert_eq!(errors, vec![0, 0, 0], "every GETATTR succeeded");
  let end = end_rx.recv_timeout(WAIT).expect("the loop ended");
  assert_eq!(end.why, EndReason::DoorbellHungUp);
  assert!(
    end.wakes >= sent,
    "one wake per kick at least: {}",
    end.wakes
  );
  assert!(
    end.passes >= sent,
    "one pass per kick at least: {}",
    end.passes
  );
  let reclaimed = end.reclaimed.expect("the terminal step ran");
  assert!(reclaimed.references_swept);
  assert_eq!(reclaimed.credits_restored, (8, 1 << 20));
  let _ = rt.shutdown();
}

/// A revoke request from the shard wakes the loop out of its wait on the doorbell and ends it
/// through the terminal step; a second request finds no loop.
#[test]
fn a_revoke_request_wakes_the_loop_and_reclaims() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let (end_tx, end_rx) = channel::<ServeEnd>();
  let (guest_tx, guest_rx) = channel::<(i32, bool, bool)>();
  rt.spawn_on(shard, async move {
    let mut owned = OwnedVolume::fresh();
    let (admitted, kick_write, call_read) = admitted_over_pipes(&mut owned.registry);
    let id = register(DEVICE_BOUND).unwrap();
    let device_task = futures::spawn(async move {
      let end = serve_loop(id, admitted, owned).await;
      let _ = end_tx.send(end);
    })
    .unwrap();
    let _ = futures::detach(device_task);
    let guest_task = futures::spawn(async move {
      let error = guest_round_trip(1, &kick_write, &call_read).await;
      let first = request_revoke(id);
      // Let the loop run its terminal step, then ask again: the loop is gone.
      futures::yield_now().await;
      futures::yield_now().await;
      let second = request_revoke(id);
      let _ = guest_tx.send((error, first, second));
    })
    .unwrap();
    let _ = futures::detach(guest_task);
  })
  .unwrap();

  let (error, first, second) = guest_rx.recv_timeout(WAIT).expect("the guest finished");
  assert_eq!(error, 0);
  assert!(first, "the loop was running when revocation was requested");
  let end = end_rx.recv_timeout(WAIT).expect("the loop ended");
  assert_eq!(end.why, EndReason::Revoked);
  assert!(end.reclaimed.is_ok());
  assert!(
    !second || end.passes > 0,
    "a second request after the end finds no loop"
  );
  let _ = rt.shutdown();
}

/// An in-process seam has no doorbell for a loop to wait on: the loop ends at once, naming it, and
/// still runs the terminal step (the in-process VMM drives service directly instead).
#[test]
fn a_seam_without_a_doorbell_ends_the_loop_at_once() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let (end_tx, end_rx) = channel::<ServeEnd>();
  rt.spawn_on(shard, async move {
    let seam = SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 0 }));
    let mut owned = OwnedVolume::fresh();
    let admitted = admit(
      request(),
      seam,
      DeviceConfig::new(FsTag::new("slates").unwrap()),
      credits(),
      rw,
      &mut owned.registry,
    )
    .unwrap();
    let id = register(DEVICE_BOUND).unwrap();
    let task = futures::spawn(async move {
      let end = serve_loop(id, admitted, owned).await;
      let _ = end_tx.send(end);
    })
    .unwrap();
    let _ = futures::detach(task);
  })
  .unwrap();
  let end = end_rx.recv_timeout(WAIT).expect("the loop ended");
  assert_eq!(end.why, EndReason::NoDoorbell);
  assert!(end.reclaimed.is_ok());
  assert_eq!((end.wakes, end.passes), (0, 0));
  let _ = rt.shutdown();
}
