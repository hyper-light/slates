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

use std::cell::{Cell, RefCell};
use std::os::fd::{AsRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use common::{SimDriver, SimVmm, message, reply_error, store, vid, volume};
use slates_bridge_core::{Attachments, Bridge, Rights, VolumeBridge};
use slates_bridge_fuse::abi::Opcode;
use slates_bridge_virtiofs::admission::{
  AdmittedDevice, Doorbell, Drained, GuestAttachRequest, GuestTransport, SeamError, ServeError,
  VmmSeam, admit,
};
use slates_bridge_virtiofs::credit::AttachmentCredits;
use slates_bridge_virtiofs::device::{DeviceConfig, FIRST_REQUEST_QUEUE, FsTag};
use slates_bridge_virtiofs::memory::{GuestMemory, GuestMemoryError, GuestRange};
use slates_bridge_virtiofs::serve::{
  BridgeAccess, DeviceId, EndReason, ServeEnd, TOLD_ABANDONED, register, request_revoke, serve_loop,
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

/// Shape: how long a fenced test owner asks the loop to wait before asking again (one timer tick of the
/// test runtime's ten), so a held request is re-asked many times within the test's wait.
const FENCE_WAIT_NS: u64 = 1_000_000;
/// Shape: the fence intervals the guest lets pass before it checks its requests are still held.
const HELD_INTERVALS: u64 = 5;
/// Shape: the bytes a DAX backing's fill, ingest or refill slice copies here: one 4 KiB page, the smallest whole slice.
const COPY_SLICE: u64 = 4096;
/// Shape: the bound the test owner names for a reply's wait on the volume's FUSE mounts (AC-3.3): long enough that a read
/// the guest sends after a write is served well within it, short enough to keep the tests quick.
const TOLD_BOUND_NS: u64 = 200_000_000;
/// Shape: the bytes the told tests' guest writes and reads back: a reply that fits [`REPLY_CAP`].
const TOLD_BYTES: usize = 64;
/// Format: the byte the told tests' file holds first, and the byte the guest writes over it.
const FIRST: u8 = 0x11;
const WRITTEN: u8 = 0x5A;
/// Format: `fuse_write_in` and `fuse_read_in` (`include/uapi/linux/fuse.h`): `fh` (8), `offset` (8), `size` (4), the
/// flags and lock owner after them, 40 bytes each, before a write's data.
const IO_IN_BYTES: usize = 40;
/// Format: the bytes of a `fuse_write_out`: the size written (4) and padding (4).
const WRITE_OUT_BYTES: usize = 8;

/// The waits on the stand-in FUSE mount alive now, for the tests that do not count them.
static TOLD_LIVE_ELSEWHERE: AtomicU64 = AtomicU64::new(0);

thread_local! {
  /// Whether the test owner is fenced (AUD-29-83): its lease does not hold, so it may not serve latest
  /// state. Set and read on the one shard by the guest task and the loop.
  static FENCED: Cell<bool> = const { Cell::new(false) };
  /// A loop the test owner asks to revoke from inside its first pass (AUD-29-87): the request arrives while
  /// the loop is serving, never while it waits.
  static REVOKE_DURING_PASS: Cell<Option<DeviceId>> = const { Cell::new(None) };
  /// The guest, shared by the device task and the guest task on the one shard — the in-process
  /// VMM's shape: one address space, two parties, never concurrent.
  static GUEST: RefCell<Option<SimDriver>> = const { RefCell::new(None) };
  /// The stand-in FUSE mount's told point (AC-3.3): every journal record through it has been told. `u64::MAX`, the
  /// default, tells every change at once, as a volume with no FUSE mount does.
  static TOLD_THROUGH: Cell<u64> = const { Cell::new(u64::MAX) };
  /// The waits parked on the stand-in mount, woken as its told point moves.
  static TOLD_WAKERS: RefCell<Vec<Waker>> = const { RefCell::new(Vec::new()) };
  /// What the loop counted on the stand-in mount: replies that waited for it, and replies published at the bound.
  static TOLD_COUNTED: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
  /// The replies the loop counted as still held for the stand-in mount when it ended ([`TOLD_ABANDONED`]).
  static ABANDONED: Cell<u64> = const { Cell::new(0) };
  /// When the guest of the guest-kill test hung up, for the loop's task to measure its end from.
  static HUNG_UP: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// The stand-in FUSE mount's wait for its told point to reach a journal record, counted alive in `live` until it is
/// dropped, so a test can hold that no wait outlives the loop that made it.
struct StandInTold {
  seq: u64,
  live: &'static AtomicU64,
}

impl StandInTold {
  fn new(seq: u64, live: &'static AtomicU64) -> StandInTold {
    live.fetch_add(1, Ordering::Relaxed);
    StandInTold { seq, live }
  }
}

impl std::future::Future for StandInTold {
  type Output = ();

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    if TOLD_THROUGH.with(Cell::get) >= self.seq {
      return Poll::Ready(());
    }
    TOLD_WAKERS.with(|wakers| {
      let mut wakers = wakers.borrow_mut();
      if !wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
        wakers.push(cx.waker().clone());
      }
    });
    Poll::Pending
  }
}

impl Drop for StandInTold {
  fn drop(&mut self) {
    self.live.fetch_sub(1, Ordering::Relaxed);
  }
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
  fn order(&self, edge: slates_bridge_virtiofs::memory::Edge) {
    with_guest(|g| g.memory.order(edge));
  }
}

/// A VMM seam over two pipes: the kick (its read end is the doorbell) and the call (its write end
/// raises the used-buffer notification).
struct PipeVmm {
  kick_read: OwnedFd,
  call_write: OwnedFd,
  memory: SharedGuestMemory,
  notified: u64,
  /// The refusal this seam's doorbell drain answers with instead of draining — a front end that broke the wire,
  /// as the vhost-user seam reports one. Persistent rather than one-shot, so a loop that fails to end on it keeps
  /// meeting it rather than quietly draining past.
  fault: Option<SeamError>,
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
    Ok(Principal::Uid {
      host: slates_db::HostId(0),
      uid: 501,
    })
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
    match self.fault.clone() {
      Some(refused) => Err(refused),
      None => drain(&self.kick_read),
    }
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

/// A non-blocking pipe pair, close-on-exec ([`close_on_exec_pipe`]): a loop that ends on the doorbell's hang-up
/// (`EndReason::DoorbellHungUp`) waits for every holder of its write end to go.
fn pipe() -> (OwnedFd, OwnedFd) {
  let (read, write) = close_on_exec_pipe();
  rustix::io::ioctl_fionbio(&read, true).unwrap();
  (read, write)
}

/// A pipe whose two ends are close-on-exec, so a child another test spawns while they are open does not keep a copy
/// past its `exec` and hold a dropped end alive (main 2e74a8af, `crates/cli/tests/cli.rs` `close_on_exec_pipe`). Linux
/// sets the flag in the same call (`pipe2`); elsewhere it is set at once after, the one window a concurrent fork can
/// still land in.
fn close_on_exec_pipe() -> (OwnedFd, OwnedFd) {
  #[cfg(target_os = "linux")]
  let ends = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
  #[cfg(not(target_os = "linux"))]
  let ends = {
    let (reader, writer) = rustix::pipe::pipe().unwrap();
    rustix::io::fcntl_setfd(&reader, rustix::io::FdFlags::CLOEXEC).unwrap();
    rustix::io::fcntl_setfd(&writer, rustix::io::FdFlags::CLOEXEC).unwrap();
    (reader, writer)
  };
  ends
}

/// The test's owned volume, lent to the loop as its bridge.
struct OwnedVolume {
  store: Store,
  volume: Volume,
  /// The owner's attachment registry the device is admitted into (GAP-A9-4).
  registry: Attachments,
  /// The barriers the loop ran (AUD-29-82); each captures (the test owner has nothing to refuse with).
  barriers: u64,
  /// Where the stand-in FUSE mount's waits are counted alive.
  told_live: &'static AtomicU64,
}

impl OwnedVolume {
  fn fresh() -> OwnedVolume {
    let mut store = store();
    let volume = volume(&mut store);
    OwnedVolume {
      store,
      volume,
      registry: Attachments::new(),
      barriers: 0,
      told_live: &TOLD_LIVE_ELSEWHERE,
    }
  }

  /// A volume holding file `f` of [`TOLD_BYTES`] bytes of [`FIRST`], its stand-in mount's waits counted in `live`: with
  /// the file's node and the journal head after it was made.
  fn with_file(live: &'static AtomicU64) -> (OwnedVolume, u64, u64) {
    let mut owned = OwnedVolume::fresh();
    owned.told_live = live;
    let root = owned.volume.root_inode(&owned.store).unwrap();
    let node = owned
      .volume
      .create_file_no(&mut owned.store, root, "f", 0o644)
      .unwrap();
    owned
      .volume
      .write(&mut owned.store, node, 0, &[FIRST; TOLD_BYTES])
      .unwrap();
    owned.volume.mark_published(&owned.store);
    let head = owned.volume.op_log().head_seq();
    (owned, node.0, head)
  }
}

impl BridgeAccess for OwnedVolume {
  fn with_bridge<R>(
    &mut self,
    f: impl FnOnce(&mut dyn Bridge, &mut Attachments) -> R,
  ) -> Result<R, VfsError> {
    if let Some(id) = REVOKE_DURING_PASS.with(Cell::take) {
      request_revoke(id);
    }
    let mut bridge = VolumeBridge::new(vid(), &mut self.volume, &mut self.store);
    Ok(f(&mut bridge, &mut self.registry))
  }

  fn fenced(&mut self) -> Option<u64> {
    FENCED.with(Cell::get).then_some(FENCE_WAIT_NS)
  }

  fn with_registry<R>(&mut self, f: impl FnOnce(&mut Attachments) -> R) -> Option<R> {
    Some(f(&mut self.registry))
  }

  fn with_backings<R>(
    &mut self,
    f: impl FnOnce(&mut slates_vfs::shards_backing::ShardsBackings) -> R,
  ) -> Option<R> {
    Some(f(&mut self.store.shards_backings))
  }

  fn copy_slice_bytes(&mut self) -> u64 {
    COPY_SLICE
  }

  fn barrier(&mut self) -> bool {
    self.barriers += 1;
    true
  }

  /// Keeps the replies abandoned with the loop (A-196). These loops serve no backed DAX, whose counts the daemon's status
  /// test holds by use (`crates/server/tests/virtiofs.rs`).
  fn count(&mut self, name: &'static str, by: u64) {
    if name == TOLD_ABANDONED {
      ABANDONED.with(|abandoned| abandoned.set(abandoned.get() + by));
    }
  }

  type Told = StandInTold;

  fn told_through(&mut self, seq: u64) -> bool {
    TOLD_THROUGH.with(Cell::get) >= seq
  }

  fn told(&mut self, seq: u64) -> StandInTold {
    StandInTold::new(seq, self.told_live)
  }

  fn told_bound_ns(&mut self) -> u64 {
    TOLD_BOUND_NS
  }

  fn told_settled(&mut self, waited: u64, overdue: u64) {
    TOLD_COUNTED.with(|counted| {
      let (before_waited, before_overdue) = counted.get();
      counted.set((before_waited + waited, before_overdue + overdue));
    });
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
  admitted_over_pipes_with_fault(registry, None)
}

/// As [`admitted_over_pipes`], with a refusal the seam's doorbell drain answers instead of draining.
fn admitted_over_pipes_with_fault(
  registry: &mut Attachments,
  fault: Option<SeamError>,
) -> (AdmittedDevice<PipeVmm>, OwnedFd, OwnedFd) {
  let (kick_read, kick_write) = pipe();
  let (call_read, call_write) = pipe();
  GUEST.with(|guest| *guest.borrow_mut() = Some(SimDriver::new(&QUEUE_SIZES)));
  let seam = PipeVmm {
    kick_read,
    call_write,
    memory: SharedGuestMemory,
    notified: 0,
    fault,
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

/// A seam refusal reaches the end record with its reason (§4.6; CLAUDE.md #9). Do: serve a device whose seam
/// answers the doorbell drain with a broken-wire refusal — what the vhost-user seam now reports for a VMM that
/// sends an oversized message or a vring address of the wrong size. Expect: the loop ends `Faulted` carrying that
/// exact refusal, not `DoorbellHungUp`.
///
/// The distinction is the point. A guest that shut down and a VMM that violated the protocol are different events
/// for whoever reads the end record, and the loop's `Faulted` arm — which has always existed — had no test and no
/// caller that could reach it: every seam collapsed its refusals into a hang-up first.
#[test]
fn a_seam_refusal_ends_the_loop_faulted_with_its_reason() {
  let refusal = SeamError::FrontEndBroke {
    reason: "a message exceeds the largest this back end accepts",
  };
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let (end_tx, end_rx) = channel::<ServeEnd>();
  let injected = refusal.clone();
  rt.spawn_on(shard, async move {
    let mut owned = OwnedVolume::fresh();
    let (admitted, kick_write, _call_read) =
      admitted_over_pipes_with_fault(&mut owned.registry, Some(injected));
    let id = register(DEVICE_BOUND).unwrap();
    let device_task = futures::spawn(async move {
      let end = serve_loop(id, admitted, owned).await;
      let _ = end_tx.send(end);
    })
    .unwrap();
    let _ = futures::detach(device_task);
    // Closing the kick's write end makes the doorbell readable, which wakes the loop into a drain. Which of the
    // two — a kick or an end-of-file — does not matter here: the seam answers the drain with its refusal before
    // it ever reads the pipe, so the wake is only how the loop gets there.
    drop(kick_write);
  })
  .unwrap();

  let end = end_rx.recv_timeout(WAIT).expect("the loop ended");
  assert_eq!(
    end.why,
    EndReason::Faulted(ServeError::Seam(refusal)),
    "a seam refusal ends the loop by name, carrying the rule the front end broke"
  );
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
  rt.shutdown().unwrap();
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
  rt.shutdown().unwrap();
}

/// Shape: how long the guest stays idle after its round trip before the shard asks for revocation, so the loop
/// has finished its pass and parked on its doorbell. The pass takes microseconds; this is far past it.
const SETTLE_NS: u64 = 20_000_000;

/// A-149 (`docs/bugs/2026-10-22-a-readiness-wait-took-any-wake-of-its-task-for-its-own.md`). A revoke request (a
/// `detach`, or the volume's destroy) reaches a loop parked on an idle guest's doorbell, with no guest kick to come.
/// Do: serve one guest round trip, let the loop park on its doorbell, then request revocation and kick nothing,
/// holding the guest's ends open so no hang-up can end the loop instead. Expect: the loop ends, revoked, through its
/// terminal step. The loop's doorbell wait was interrupted by the revoke's wake only while a readiness wait took
/// any wake of its task for its own; once it named its own descriptor, such a loop kept serving a detached device
/// until the guest kicked again.
#[test]
fn a_revoke_request_ends_a_loop_parked_on_an_idle_guests_doorbell() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let (end_tx, end_rx) = channel::<ServeEnd>();
  let (guest_tx, guest_rx) = channel::<(i32, bool)>();
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
      // The guest goes idle, and the loop finishes its pass and parks on the doorbell.
      let _ = futures::sleep(SETTLE_NS).await;
      let asked = request_revoke(id);
      let _ = guest_tx.send((error, asked));
      // The guest's ends stay open until the runtime's shutdown cancels this task, so only the revoke can end the
      // loop: a closed kick would end it as a hang-up (and a hold of the test's own wait raced it, 2026-10-22).
      futures::idle().await;
      drop((kick_write, call_read));
    })
    .unwrap();
    let _ = futures::detach(guest_task);
  })
  .unwrap();

  let (error, asked) = guest_rx.recv_timeout(WAIT).expect("the guest finished");
  assert_eq!(error, 0);
  assert!(asked, "the loop was running when revocation was requested");
  let end = end_rx
    .recv_timeout(WAIT)
    .expect("the revoke ended the loop parked on its doorbell, with no guest kick");
  assert_eq!(end.why, EndReason::Revoked);
  assert!(end.reclaimed.is_ok());
  rt.shutdown().unwrap();
}

/// An in-process seam has no doorbell for a loop to wait on: the loop ends at once, naming it, and
/// still runs the terminal step (the in-process VMM drives service directly instead).
#[test]
fn a_seam_without_a_doorbell_ends_the_loop_at_once() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let (end_tx, end_rx) = channel::<ServeEnd>();
  rt.spawn_on(shard, async move {
    let seam = SimVmm::new(
      &QUEUE_SIZES,
      Ok(Principal::Uid {
        host: slates_db::HostId(0),
        uid: 0,
      }),
    );
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
  rt.shutdown().unwrap();
}

/// The guest submits `requests` GETATTRs of the root at once and kicks once; returns their heads.
fn submit_getattrs(requests: u64, kick_write: &OwnedFd) -> Vec<u16> {
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  let heads = (0..requests)
    .map(|unique| {
      with_guest(|g| {
        g.submit(
          rq,
          &message(Opcode::GetAttr.to_wire(), 100 + unique, 1, &[0u8; 16]),
          REPLY_CAP,
          1,
        )
      })
    })
    .collect();
  rustix::io::write(kick_write, &[1u8]).unwrap();
  heads
}

/// AUD-29-83 (§4.8 "Leases and reads"). Do: fence the owner (its lease does not hold), have the guest submit a
/// GETATTR and kick, let several fence intervals pass, then lift the fence. Expect: while fenced no used element
/// is published — the request waits in the ring, never answered from a stale owner — and the loop counted its
/// fenced waits; once the lease holds, the same request is answered and notified.
#[test]
fn a_fenced_owner_holds_the_guests_requests_until_its_lease_holds() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let (end_tx, end_rx) = channel::<ServeEnd>();
  let (guest_tx, guest_rx) = channel::<(bool, i32)>();
  rt.spawn_on(shard, async move {
    FENCED.with(|fenced| fenced.set(true));
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
      let rq = usize::from(FIRST_REQUEST_QUEUE);
      let heads = submit_getattrs(1, &kick_write);
      futures::sleep(FENCE_WAIT_NS * HELD_INTERVALS)
        .await
        .unwrap();
      let held = with_guest(|g| g.reap(rq)).is_none();
      FENCED.with(|fenced| fenced.set(false));
      loop {
        readable(call_read.as_raw_fd()).await.unwrap();
        if drain(&call_read).unwrap() == Drained::Kicked {
          break;
        }
      }
      let (id, len) = with_guest(|g| g.reap(rq)).expect("answered once the lease holds");
      assert_eq!(id, heads[0]);
      let error = with_guest(|g| reply_error(&g.reply_of(rq, id, len)));
      drop(kick_write);
      let _ = guest_tx.send((held, error));
    })
    .unwrap();
    let _ = futures::detach(guest_task);
  })
  .unwrap();
  let (held, error) = guest_rx.recv_timeout(WAIT).expect("the guest finished");
  assert!(held, "no used element while the owner was fenced");
  assert_eq!(error, 0, "answered once the lease held");
  let end = end_rx.recv_timeout(WAIT).expect("the loop ended");
  assert!(end.fenced_waits > 0, "the loop held the request: {end:?}");
  rt.shutdown().unwrap();
}

/// Shape: requests the guest keeps in its ring for the revocation test — the request queue's 32 descriptors
/// hold 16 two-part chains, two passes' worth at the test's request credit (8 per pass), so a loop that
/// ignored the request would serve both passes.
const FULL_RING: u64 = 16;

/// AUD-29-87. Do: the guest fills its ring with 16 GETATTRs and kicks once; the owner asks for revocation from
/// inside the loop's first pass. Expect: the loop ends `Revoked` at the next pass boundary — one pass, the
/// first pass's 8 requests answered and the other 8 never touched — and the terminal step runs. Before, the
/// inner loop served until the ring was idle (two passes, all 16) before it looked.
#[test]
fn a_revoke_requested_mid_service_ends_the_loop_at_the_next_pass() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let (end_tx, end_rx) = channel::<(ServeEnd, usize)>();
  rt.spawn_on(shard, async move {
    let mut owned = OwnedVolume::fresh();
    let (admitted, kick_write, call_read) = admitted_over_pipes(&mut owned.registry);
    let id = register(DEVICE_BOUND).unwrap();
    REVOKE_DURING_PASS.with(|revoke| revoke.set(Some(id)));
    submit_getattrs(FULL_RING, &kick_write);
    let device_task = futures::spawn(async move {
      let end = serve_loop(id, admitted, owned).await;
      let answered = with_guest(|g| g.used(usize::from(FIRST_REQUEST_QUEUE)).len());
      drop((kick_write, call_read));
      let _ = end_tx.send((end, answered));
    })
    .unwrap();
    let _ = futures::detach(device_task);
  })
  .unwrap();
  let (end, answered) = end_rx.recv_timeout(WAIT).expect("the loop ended");
  assert_eq!(end.why, EndReason::Revoked);
  assert_eq!(end.passes, 1, "no pass after the request: {end:?}");
  assert_eq!(answered, 8, "the first pass's requests, and no more");
  assert!(end.reclaimed.is_ok());
  rt.shutdown().unwrap();
}

/// A `FUSE_WRITE` of [`TOLD_BYTES`] bytes of [`WRITTEN`] at the start of node `node`.
fn write_request(unique: u64, node: u64) -> Vec<u8> {
  let mut body = Vec::new();
  body.extend_from_slice(&0u64.to_le_bytes());
  body.extend_from_slice(&0u64.to_le_bytes());
  body.extend_from_slice(&u32::try_from(TOLD_BYTES).unwrap().to_le_bytes());
  body.resize(IO_IN_BYTES, 0);
  body.extend_from_slice(&[WRITTEN; TOLD_BYTES]);
  message(Opcode::Write.to_wire(), unique, node, &body)
}

/// A `FUSE_READ` of [`TOLD_BYTES`] bytes at the start of node `node`.
fn read_request(unique: u64, node: u64) -> Vec<u8> {
  let mut body = Vec::new();
  body.extend_from_slice(&0u64.to_le_bytes());
  body.extend_from_slice(&0u64.to_le_bytes());
  body.extend_from_slice(&u32::try_from(TOLD_BYTES).unwrap().to_le_bytes());
  body.resize(IO_IN_BYTES, 0);
  message(Opcode::Read.to_wire(), unique, node, &body)
}

/// Waits for the device's next used-buffer notification.
async fn notified(call_read: &OwnedFd) {
  loop {
    readable(call_read.as_raw_fd()).await.unwrap();
    if drain(call_read).unwrap() == Drained::Kicked {
      break;
    }
  }
}

/// What the guest saw of a write to `f` and a read of it sent after: the write's chain head, and the read's used
/// element (its head, its errno and its data) and how long after the write was sent it was published.
struct ReadAfterWrite {
  write: u16,
  read: (u16, i32, Vec<u8>),
  read_after: Duration,
  sent: Instant,
}

/// The guest writes node `node`, lets the loop serve it, then reads it back, and takes the first used element the
/// device publishes after that.
async fn read_after_write(node: u64, kick: &OwnedFd, call: &OwnedFd) -> ReadAfterWrite {
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  let sent = Instant::now();
  let write = with_guest(|g| g.submit(rq, &write_request(1, node), REPLY_CAP, 1));
  rustix::io::write(kick, &[1u8]).unwrap();
  // The loop runs on this shard, so it serves the write once this task yields to it.
  let _ = futures::yield_now().await;
  let read = with_guest(|g| g.submit(rq, &read_request(2, node), REPLY_CAP, 1));
  rustix::io::write(kick, &[1u8]).unwrap();
  notified(call).await;
  let (head, len) = with_guest(|g| g.reap(rq)).expect("a used element after the notification");
  let reply = with_guest(|g| g.reply_of(rq, head, len));
  assert_eq!(
    head, read,
    "the read's element is published first, while the write's waits"
  );
  ReadAfterWrite {
    write,
    read: (
      head,
      reply_error(&reply),
      reply.get(OUT_HEADER..).unwrap_or(&[]).to_vec(),
    ),
    read_after: sent.elapsed(),
    sent,
  }
}

/// Format: the bytes of a `fuse_out_header`.
const OUT_HEADER: usize = 16;

/// AC-3.3 for a guest writer, with a FUSE mount whose round is held (A-196). Do: over a volume holding file `f`, whose
/// one FUSE mount (a stand-in) has been told through every record so far and is told nothing more, the guest writes
/// `f` and then reads it back; then it hangs up. Expect: the read's used element is published first, its data the
/// written bytes, while the write's waits; the write's is published only once the bound has passed since it was sent,
/// counted overdue and never held beyond; the loop's wait on the mount is gone once the reply is out; the terminal step
/// restores every credit.
#[test]
fn a_guests_write_waits_for_a_held_fuse_mount_to_the_bound_while_a_later_read_completes() {
  static LIVE: AtomicU64 = AtomicU64::new(0);
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let (end_tx, end_rx) = channel::<(ServeEnd, u64)>();
  let (guest_tx, guest_rx) = channel::<(
    ReadAfterWrite,
    Duration,
    (i32, Option<u32>),
    (u64, u64),
    u64,
  )>();
  rt.spawn_on(shard, async move {
    let (mut owned, node, head) = OwnedVolume::with_file(&LIVE);
    // The stand-in mount was told through every record so far, and its round for the guest's write is held.
    TOLD_THROUGH.with(|told| told.set(head));
    let (admitted, kick_write, call_read) = admitted_over_pipes(&mut owned.registry);
    let id = register(DEVICE_BOUND).unwrap();
    let device_task = futures::spawn(async move {
      let end = serve_loop(id, admitted, owned).await;
      let _ = end_tx.send((end, ABANDONED.with(Cell::get)));
    })
    .unwrap();
    let _ = futures::detach(device_task);
    let guest_task = futures::spawn(async move {
      let seen = read_after_write(node, &kick_write, &call_read).await;
      notified(&call_read).await;
      let rq = usize::from(FIRST_REQUEST_QUEUE);
      let (head, len) = with_guest(|g| g.reap(rq)).expect("the write's element, at the bound");
      assert_eq!(
        head, seen.write,
        "the second element published is the write's"
      );
      let write_after = seen.sent.elapsed();
      let reply = with_guest(|g| g.reply_of(rq, head, len));
      let wrote = reply
        .get(OUT_HEADER..OUT_HEADER + WRITE_OUT_BYTES)
        .and_then(|out| out.get(..4))
        .and_then(|size| size.try_into().ok())
        .map(u32::from_le_bytes);
      let live = LIVE.load(Ordering::Relaxed);
      drop(kick_write);
      let answer = (reply_error(&reply), wrote);
      let _ = guest_tx.send((
        seen,
        write_after,
        answer,
        TOLD_COUNTED.with(Cell::get),
        live,
      ));
    })
    .unwrap();
    let _ = futures::detach(guest_task);
  })
  .unwrap();

  let (seen, write_after, (errno, wrote), counted, live) =
    guest_rx.recv_timeout(WAIT).expect("the guest finished");
  println!(
    "the read was published {:?} after the write was sent, the write {write_after:?}",
    seen.read_after
  );
  assert_eq!(
    (seen.read.1, seen.read.2.as_slice()),
    (0, &[WRITTEN; TOLD_BYTES][..]),
    "the read succeeded and saw the write, applied before its reply"
  );
  assert!(
    write_after >= Duration::from_nanos(TOLD_BOUND_NS),
    "the write's reply waited the bound: {write_after:?}"
  );
  assert_eq!(errno, 0, "the write succeeded");
  assert_eq!(
    wrote,
    u32::try_from(TOLD_BYTES).ok(),
    "the write's reply reports every byte written"
  );
  assert_eq!(counted, (0, 1), "counted overdue, not waited");
  assert_eq!(
    live, 0,
    "the loop's wait on the mount is gone once the reply is out"
  );
  let (end, abandoned) = end_rx.recv_timeout(WAIT).expect("the loop ended");
  assert_eq!(end.why, EndReason::DoorbellHungUp);
  assert_eq!(
    abandoned, 0,
    "no reply was still held at the end: the write's went out at the bound"
  );
  let reclaimed = end.reclaimed.expect("the terminal step ran");
  assert_eq!(reclaimed.credits_restored, (8, 1 << 20));
  rt.shutdown().unwrap();
}

/// AC-3.3, chaos (A-196): the guest killed while its write's reply waits for a FUSE mount whose round is held. Do: the
/// guest writes `f` and reads it back, served at once, then hangs up while the write's reply still waits. Expect: the
/// loop ends on the hangup at once, not at the bound; its wait on the mount is gone; the terminal step restores every
/// credit; and the write's element was never published while the read showed its bytes: applied, never half.
#[test]
fn a_guest_killed_while_its_writes_reply_waits_ends_the_loop_and_leaks_nothing() {
  static LIVE: AtomicU64 = AtomicU64::new(0);
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let (end_tx, end_rx) = channel::<(ServeEnd, bool, Duration, u64)>();
  let (guest_tx, guest_rx) = channel::<(ReadAfterWrite, Instant)>();
  rt.spawn_on(shard, async move {
    let (mut owned, node, head) = OwnedVolume::with_file(&LIVE);
    TOLD_THROUGH.with(|told| told.set(head));
    let (admitted, kick_write, call_read) = admitted_over_pipes(&mut owned.registry);
    let id = register(DEVICE_BOUND).unwrap();
    let device_task = futures::spawn(async move {
      let end = serve_loop(id, admitted, owned).await;
      let never_published = with_guest(|g| g.reap(usize::from(FIRST_REQUEST_QUEUE))).is_none();
      let after = HUNG_UP
        .with(Cell::get)
        .map_or(Duration::MAX, |at| at.elapsed());
      let _ = end_tx.send((end, never_published, after, ABANDONED.with(Cell::get)));
    })
    .unwrap();
    let _ = futures::detach(device_task);
    let guest_task = futures::spawn(async move {
      let seen = read_after_write(node, &kick_write, &call_read).await;
      let at = Instant::now();
      HUNG_UP.with(|hung_up| hung_up.set(Some(at)));
      drop(kick_write);
      let _ = guest_tx.send((seen, at));
    })
    .unwrap();
    let _ = futures::detach(guest_task);
  })
  .unwrap();

  let (seen, _at) = guest_rx
    .recv_timeout(WAIT)
    .expect("the guest wrote, read and hung up");
  assert_eq!(
    seen.read.2.as_slice(),
    &[WRITTEN; TOLD_BYTES][..],
    "the read saw the write: it was applied"
  );
  let (end, never_published, after, abandoned) = end_rx.recv_timeout(WAIT).expect("the loop ended");
  assert_eq!(end.why, EndReason::DoorbellHungUp);
  assert_eq!(
    abandoned, 1,
    "the loop counted the write's reply abandoned with it"
  );
  assert!(
    after < Duration::from_nanos(TOLD_BOUND_NS),
    "the loop ended on the hangup, not at the bound: {after:?}"
  );
  assert!(
    never_published,
    "the held write's element was never published"
  );
  assert_eq!(
    LIVE.load(Ordering::Relaxed),
    0,
    "no wait on the mount outlives the loop"
  );
  let reclaimed = end.reclaimed.expect("the terminal step ran");
  assert_eq!(
    reclaimed.credits_restored,
    (8, 1 << 20),
    "every credit restored, the held chain's too"
  );
  rt.shutdown().unwrap();
}

/// AC-3.3, chaos (A-196): the daemon killed while a guest write's reply waits. Do: as above, but the runtime that runs
/// the loop shuts down while the write's reply waits, as a killed daemon's goes. Expect: the loop's wait on the mount
/// goes with it (none alive afterwards), the loop never reports an end of its own, and the write was applied (the read
/// showed it) while its element was never published.
#[test]
fn a_daemon_stopped_while_a_guest_writes_reply_waits_leaves_no_wait_behind() {
  static LIVE: AtomicU64 = AtomicU64::new(0);
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let (end_tx, end_rx) = channel::<ServeEnd>();
  let (guest_tx, guest_rx) = channel::<(ReadAfterWrite, u64)>();
  rt.spawn_on(shard, async move {
    let (mut owned, node, head) = OwnedVolume::with_file(&LIVE);
    TOLD_THROUGH.with(|told| told.set(head));
    let (admitted, kick_write, call_read) = admitted_over_pipes(&mut owned.registry);
    let id = register(DEVICE_BOUND).unwrap();
    let device_task = futures::spawn(async move {
      let end = serve_loop(id, admitted, owned).await;
      let _ = end_tx.send(end);
    })
    .unwrap();
    let _ = futures::detach(device_task);
    let guest_task = futures::spawn(async move {
      let seen = read_after_write(node, &kick_write, &call_read).await;
      // The loop is waiting on the held write's mount now: its wait is alive.
      let live = LIVE.load(Ordering::Relaxed);
      let _ = guest_tx.send((seen, live));
      // Keep the guest alive across the stop: only the daemon goes.
      let _kept = kick_write;
      let _ = futures::idle().await;
    })
    .unwrap();
    let _ = futures::detach(guest_task);
  })
  .unwrap();

  let (seen, live_before) = guest_rx
    .recv_timeout(WAIT)
    .expect("the guest wrote and read");
  assert_eq!(
    seen.read.2.as_slice(),
    &[WRITTEN; TOLD_BYTES][..],
    "the write was applied"
  );
  assert_eq!(live_before, 1, "the loop waits on the held write's mount");
  rt.shutdown().unwrap();
  assert_eq!(
    LIVE.load(Ordering::Relaxed),
    0,
    "the stop dropped the loop's wait with it"
  );
  assert!(
    end_rx.recv_timeout(Duration::from_millis(1)).is_err(),
    "the loop never ended on its own: the stop took it"
  );
}
