//! The device loop on the runtime (§4.3; §4.6 A-9: "The guest transport is FUSE-over-virtio
//! served by an owned device integrated with the custom executor"; D-9: `slates-rt` is the only
//! runtime). An admitted device is served by one perpetual task on the volume's owning shard — the
//! shape of the daemon's NFS serve (`crates/server/src/nfs.rs`): a `Send` boot task reaches the
//! shard through `spawn_on`, builds the device over the shard's own state, and `futures::spawn`s
//! this non-`Send` loop locally. The loop awaits the seam's doorbell descriptor through the shard's
//! driver ([`slates_rt::readiness::readable`], the same edge a socket is awaited on), asks the seam
//! to drain it, runs service passes until the rings are idle (yielding between passes so a busy
//! guest never runs past the shard's step budget), and repeats. It never polls: a shard with an
//! idle guest parks.
//!
//! The loop settles the replies the device holds for the volume's FUSE mounts (AC-3.3, A-191,
//! A-196): a write's reply waits until every FUSE mount of the volume has been told of the write,
//! and the device serves the guest's later requests meanwhile, completing the held reply out of
//! order, which virtio permits (virtio 1.2 §2.6; the device offers no `VIRTIO_F_IN_ORDER`). After
//! every pass, and whenever the oldest held reply's wait on the mounts or its bound wakes it while
//! the guest is idle, the loop publishes each held reply whose mounts were told and each that has
//! waited the bound, counted overdue. Only this loop turns holding on
//! ([`AdmittedDevice::hold_replies_for_told`]), so a caller that never settles never strands a
//! reply; one still held when the loop ends goes unsent with the device, counted
//! [`TOLD_ABANDONED`].
//!
//! The loop owns the device, so revocation reaches it as a message: [`request_revoke`] marks the
//! loop's entry in a per-shard, thread-local control map (no lock, D-7) and wakes the loop with
//! the waker it registered, and the loop runs the owned terminal step ([`AdmittedDevice::reclaim`])
//! before it ends. The VMM closing its end of the doorbell — a guest torn down — is the same
//! revocation. A device fault ends the loop the same way; the terminal step always runs, and the
//! [`ServeEnd`] says why and what was reclaimed. The control map is bounded by the caller's device
//! limit (the daemon's admission limit); registering past it is a typed refusal.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use slates_bridge_core::Attachments;
use slates_bridge_core::Bridge;
use slates_rt::error::RtError;
use slates_rt::futures;
use slates_rt::readiness::readable;
use slates_vfs::error::VfsError;

use crate::admission::{
  AdmittedDevice, Doorbell, Drained, ReclaimError, Reclaimed, ServeError, VmmSeam,
};

/// Format: the status counter of guest replies still held for the volume's FUSE mounts when their device ended (AC-3.3,
/// A-196). Each one's write was applied; its reply goes unsent with the device, as every reply in flight at an end does.
pub const TOLD_ABANDONED: &str = "virtiofs.told_abandoned";

/// The identity of one device loop on this shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeviceId(u64);

/// A loop's control entry: the waker to interrupt it with, whether revocation was asked for, and whether the
/// attachment's view moved (a *kick*, distinct from a revoke — the loop marks its backings stale and keeps serving).
struct LoopControl {
  waker: Option<Waker>,
  revoke: bool,
  advanced: bool,
}

thread_local! {
  /// The device loops this shard runs, by id; only this shard's thread touches it (D-7).
  static CONTROL: RefCell<BTreeMap<u64, LoopControl>> = const { RefCell::new(BTreeMap::new()) };
  /// The next device id this shard hands out.
  static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

/// A typed refusal from the loop registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopError {
  /// The shard already runs `bound` device loops (the caller's device limit).
  TooManyDevices {
    /// The bound.
    bound: usize,
  },
}

impl fmt::Display for LoopError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::TooManyDevices { bound } => write!(f, "this shard already serves {bound} devices"),
    }
  }
}

impl std::error::Error for LoopError {}

/// Registers a device loop on this shard under `bound` concurrent loops; the id it will run as.
pub fn register(bound: usize) -> Result<DeviceId, LoopError> {
  CONTROL.with(|control| {
    let mut control = control.borrow_mut();
    if control.len() >= bound {
      return Err(LoopError::TooManyDevices { bound });
    }
    let id = NEXT_ID.with(|next| {
      let id = next.get();
      next.set(id.wrapping_add(1));
      id
    });
    control.insert(
      id,
      LoopControl {
        waker: None,
        revoke: false,
        advanced: false,
      },
    );
    Ok(DeviceId(id))
  })
}

/// Asks the loop `id` to revoke its device and end: marks it and wakes it. `false` when no such
/// loop runs on this shard (it ended, or never started here).
pub fn request_revoke(id: DeviceId) -> bool {
  CONTROL.with(|control| {
    let mut control = control.borrow_mut();
    let Some(entry) = control.get_mut(&id.0) else {
      return false;
    };
    entry.revoke = true;
    if let Some(waker) = entry.waker.take() {
      waker.wake();
    }
    true
  })
}

/// Kicks the loop `id` because its attachment's view moved (§4.6 "revoke on advance": a snapshot reader's `advance`),
/// without a guest doorbell kick and without ending it: on its next turn the loop marks every backing stale, so each is
/// refilled from the new view before the guest sees anything more. `false` when no such loop runs on this shard.
/// Idempotent: a second kick before the loop takes it is folded into the one flag.
pub fn kick(id: DeviceId) -> bool {
  CONTROL.with(|control| {
    let mut control = control.borrow_mut();
    let Some(entry) = control.get_mut(&id.0) else {
      return false;
    };
    entry.advanced = true;
    if let Some(waker) = entry.waker.take() {
      waker.wake();
    }
    true
  })
}

/// Reads and clears the view-moved mark for loop `id` (a kick); whether one was pending.
fn take_advance_requested(id: DeviceId) -> bool {
  CONTROL.with(|control| {
    control
      .borrow_mut()
      .get_mut(&id.0)
      .is_some_and(|e| std::mem::take(&mut e.advanced))
  })
}

/// Whether revocation was asked for.
fn revoke_requested(id: DeviceId) -> bool {
  CONTROL.with(|control| control.borrow().get(&id.0).is_some_and(|e| e.revoke))
}

/// Forgets a loop that ended.
fn unregister(id: DeviceId) {
  CONTROL.with(|control| {
    control.borrow_mut().remove(&id.0);
  });
}

/// Runs `work` for loop `id` until it completes or the shard asks the loop to revoke ([`request_revoke`]): its output,
/// or `None` when the revoke came first. A device registered before it serves (a vhost-user front end still to
/// negotiate, whose end a client holds) is ended by a `detach` this way; `work` is dropped with what it owns.
pub async fn until_revoked<F: Future>(id: DeviceId, work: F) -> Option<F::Output> {
  let mut work = std::pin::pin!(work);
  std::future::poll_fn(|cx| {
    CONTROL.with(|control| {
      if let Some(entry) = control.borrow_mut().get_mut(&id.0) {
        entry.waker = Some(cx.waker().clone());
      }
    });
    if revoke_requested(id) {
      return Poll::Ready(None);
    }
    work.as_mut().poll(cx).map(Some)
  })
  .await
}

/// Forgets loop `id`, which ends before its device was ever served (its front end never negotiated, or its admission
/// was refused): [`serve_loop`] forgets the loops it ran.
pub fn abandon(id: DeviceId) {
  unregister(id);
}

/// Whether the shard has asked something of loop `id` that it has not taken yet: a revoke, or a kick for a moved view.
fn request_pending(id: DeviceId) -> bool {
  CONTROL.with(|control| {
    control
      .borrow()
      .get(&id.0)
      .is_some_and(|e| e.revoke || e.advanced)
  })
}

/// What ended a loop's wait on its doorbell.
enum Woke {
  /// The guest kicked the doorbell (or hung it up: the drain tells which).
  Doorbell,
  /// The shard asked for a revoke or a drain ([`request_revoke`], [`kick`]).
  Request,
  /// The oldest reply waiting for the volume's FUSE mounts may be published: its mounts were told, or its bound passed.
  Told,
}

/// Waits until the guest kicks the doorbell or the shard asks something of the loop. A request wakes the waker
/// [`RegisterWaker`] recorded (this task's), and is seen on the poll that follows. The doorbell's readiness answers
/// only for the doorbell, so a request is never taken for a guest kick, nor a kick for a request (A-149). Before
/// A-149 a readiness wait answered on any wake of its task, and the request's wake passed for a doorbell kick: the
/// loop relied on that to be interrupted, which is why the race is explicit now.
async fn doorbell_or_request(id: DeviceId, raw: i32) -> Result<Woke, slates_rt::RtError> {
  let mut doorbell = std::pin::pin!(readable(raw));
  std::future::poll_fn(|cx| {
    if request_pending(id) {
      return Poll::Ready(Ok(Woke::Request));
    }
    doorbell
      .as_mut()
      .poll(cx)
      .map(|ready| ready.map(|()| Woke::Doorbell))
  })
  .await
}

/// Waits as [`doorbell_or_request`] does, and, while a reply waits for the volume's FUSE mounts (AC-3.3), until `told`
/// — the oldest such reply's wait — is ready or its bound's remaining `left_ns` passes.
async fn doorbell_request_or_told<T: Future<Output = ()>>(
  id: DeviceId,
  raw: i32,
  told: Option<(T, u64)>,
) -> Result<Woke, slates_rt::RtError> {
  let Some((told, left_ns)) = told else {
    return doorbell_or_request(id, raw).await;
  };
  let mut told = std::pin::pin!(told);
  let mut either = std::pin::pin!(doorbell_or_request(id, raw));
  let raced = futures::within(
    left_ns,
    std::future::poll_fn(|cx| {
      if let Poll::Ready(woke) = either.as_mut().poll(cx) {
        return Poll::Ready(woke);
      }
      told.as_mut().poll(cx).map(|()| Ok(Woke::Told))
    }),
  )
  .await;
  match raced {
    Ok(Some(woke)) => woke,
    // The bound passed: the settle that follows publishes the reply, overdue.
    Ok(None) => Ok(Woke::Told),
    Err(refused) => Err(refused),
  }
}

/// Publishes the device's held replies whose volume's FUSE mounts were told, or whose bound passed (AC-3.3), and counts
/// what it did.
fn settle_told<S: VmmSeam, B: BridgeAccess>(
  admitted: &mut AdmittedDevice<S>,
  bridge: &mut B,
) -> Result<(), ServeError> {
  if admitted.told_oldest().is_none() {
    return Ok(());
  }
  let bound = bridge.told_bound_ns();
  let settled = admitted.settle_told(
    &mut |seq| bridge.told_through(seq),
    (futures::now_ns(), bound),
  )?;
  if settled.waited > 0 || settled.overdue > 0 {
    bridge.told_settled(settled.waited, settled.overdue);
  }
  Ok(())
}

/// The reason a loop ends on a refusal of its service.
fn end_of(refusal: ServeError) -> EndReason {
  match refusal {
    ServeError::Revoked => EndReason::Revoked,
    ServeError::Authority(VfsError::NotFound) => EndReason::VolumeGone(VfsError::NotFound),
    other => EndReason::Faulted(other),
  }
}

/// A future that records the loop's current waker in its control entry, so a revoke request or a drain kick can
/// interrupt the loop's wait on the doorbell ([`doorbell_or_request`]).
struct RegisterWaker {
  id: DeviceId,
}

impl Future for RegisterWaker {
  type Output = ();

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    let id = self.id;
    CONTROL.with(|control| {
      if let Some(entry) = control.borrow_mut().get_mut(&id.0) {
        entry.waker = Some(cx.waker().clone());
      }
    });
    Poll::Ready(())
  }
}

/// How the loop reaches the bridge for a pass: the daemon builds a transient `VolumeBridge` over
/// the shard's state; a test lends an owned volume. Refused typed when the volume is gone
/// (destroyed under a live device), which ends the loop.
pub trait BridgeAccess {
  /// Runs `f` with the bridge and the owner's attachment registry — the one every transport on the
  /// volume rides, so the owner's barriers see the device's requests (GAP-A9-4) — or refuses when
  /// there is no volume to bridge to.
  fn with_bridge<R>(
    &mut self,
    f: impl FnOnce(&mut dyn Bridge, &mut Attachments) -> R,
  ) -> Result<R, VfsError>;

  /// The owner's barrier (§4.8, D-18; AUD-29-82): makes the volume's current state survive a daemon restart
  /// — the daemon publishes the shard's recovery image — and says whether the volume was captured. Run
  /// between a mutation's dispatch and the publication of its used element, outside [`Self::with_bridge`]
  /// (the publication needs the owner's whole state). `false` — refused, or the volume left out — answers
  /// the guest `EIO`, never a promise of survival.
  fn barrier(&mut self) -> bool;

  /// Whether the owner may not serve the volume's latest state now (§4.8 "Leases and reads"; AUD-29-83): its
  /// configuration group is not ready, or its owner lease does not hold (unconfirmed, or superseded by a
  /// configuration it has not installed). `Some(wait)` holds the guest's requests in its rings — a pause, never
  /// a stale answer, as a hard NFS mount retries `NFS3ERR_JUKEBOX` — and names how long the loop waits before
  /// asking again (the interval the owner's confirmations arrive at); `None` serves.
  fn fenced(&mut self) -> Option<u64>;

  /// Runs `f` with the owner's attachment registry alone — reachable when the volume is not (destroyed under
  /// the device), so the terminal step can still withdraw the device's attachment (AUD-29-70). `None` only
  /// when the owner's state is out of reach.
  fn with_registry<R>(&mut self, f: impl FnOnce(&mut Attachments) -> R) -> Option<R>;

  /// Runs `f` with the store's DAX backings alone — reachable when the volume is not, so the terminal step of a device
  /// whose volume is gone still lets its backings go and gives their charge back (A-163). `None` only when the owner's
  /// state is out of reach.
  fn with_backings<R>(
    &mut self,
    f: impl FnOnce(&mut slates_vfs::shards_backing::ShardsBackings) -> R,
  ) -> Option<R>;

  /// The bytes one slice of copy-bound DAX work (a backing's fill, ingest or refill) may copy on this shard now: the
  /// owner's copy slice in whole pages (half the live step quantum at the measured memcpy rate), at least one page.
  /// Asked each pass, since the step quantum is live.
  fn copy_slice_bytes(&mut self) -> u64;

  /// Adds `by` to the owner's status counter `name`: what the device's backed DAX did since the loop last reported it
  /// (`BackedCounts::named`, Linux), so a guest's use of its window shows while the device runs.
  fn count(&mut self, name: &'static str, by: u64);

  /// What a reply waiting for the volume's FUSE mounts waits on (AC-3.3, A-191): a future ready once every FUSE mount of
  /// the volume has been told through a journal record. It holds no borrow of the access, and dropping it leaves
  /// nothing parked.
  type Told: Future<Output = ()> + 'static;

  /// Whether every FUSE mount of the volume has been told through journal record `seq`: `true` where the volume has none,
  /// or where no mount's kernel holds anything yet.
  fn told_through(&mut self, seq: u64) -> bool;

  /// A wait for every FUSE mount of the volume to have been told through journal record `seq`.
  fn told(&mut self, seq: u64) -> Self::Told;

  /// How long a reply waits for the volume's FUSE mounts before it is published anyway, counted overdue: the owner's
  /// bound for every writer that holds no kernel lock (A-191).
  fn told_bound_ns(&mut self) -> u64;

  /// Counts what one settle of the device's held replies did: replies that waited for the volume's FUSE mounts to be
  /// told, and replies published at the bound with a mount untold.
  fn told_settled(&mut self, waited: u64, overdue: u64);
}

/// Why the loop ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
  /// [`request_revoke`] was called.
  Revoked,
  /// The VMM closed its end of the doorbell.
  DoorbellHungUp,
  /// The device faulted (a malformed chain, a credit, the seam).
  Faulted(ServeError),
  /// The shard's driver could not wait on the doorbell.
  DoorbellLost(RtError),
  /// The seam has no descriptor doorbell: the in-process VMM drives service itself, so there is
  /// nothing for a loop to wait on.
  NoDoorbell,
  /// The shard's timer refused the wait of a fenced owner (off a shard), so the loop could not hold.
  WaitLost(RtError),
  /// The volume the device served is gone (destroyed under the device).
  VolumeGone(VfsError),
}

/// What the loop reports when it ends: why, the terminal step's outcome, and its counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServeEnd {
  /// Why.
  pub why: EndReason,
  /// The terminal step's outcome.
  pub reclaimed: Result<Reclaimed, ReclaimError>,
  /// Doorbell wakes the loop took.
  pub wakes: u64,
  /// Service passes the loop ran.
  pub passes: u64,
  /// Intervals the loop held the guest's requests because the owner was fenced (AUD-29-83).
  pub fenced_waits: u64,
}

/// Runs service passes until the rings are idle, yielding between passes; `Some(reason)` when the loop must
/// end. Revocation is checked at every pass boundary (AUD-29-87): a guest that keeps its rings full yields
/// between passes but never postpones a requested revoke past the pass in hand. While the owner is fenced
/// ([`BridgeAccess::fenced`]) no pass runs: the requests wait in the rings, re-asked each interval, and a
/// revoke or the guest's hangup is still seen within one interval.
async fn serve_until_idle<S: VmmSeam, B: BridgeAccess>(
  id: DeviceId,
  admitted: &mut AdmittedDevice<S>,
  bridge: &mut B,
  counters: &mut LoopCounters,
) -> Option<EndReason> {
  loop {
    if revoke_requested(id) {
      return Some(EndReason::Revoked);
    }
    if let Some(wait) = bridge.fenced() {
      counters.fenced_waits = counters.fenced_waits.saturating_add(1);
      if let Err(refused) = futures::sleep(wait).await {
        return Some(EndReason::WaitLost(refused));
      }
      // A guest torn down while its requests wait is seen here too: the held requests are served once the
      // fence lifts whatever kicks arrived meanwhile, so draining them loses nothing.
      match admitted.drain_doorbell() {
        Ok(Drained::HungUp) => return Some(EndReason::DoorbellHungUp),
        Ok(Drained::Kicked | Drained::Nothing) => {}
        Err(e) => return Some(EndReason::Faulted(ServeError::Seam(e))),
      }
      continue;
    }
    match one_pass(admitted, bridge, counters).and_then(|more| {
      // A reply waiting for the volume's FUSE mounts is published as soon as they are told, often within the pass
      // (AC-3.3).
      settle_told(admitted, bridge).map(|()| more)
    }) {
      Ok(more) => {
        // A message-based SETUPMAPPING may have parked for the front end's map-acks (§4.6); await them and answer
        // the guest, and settle any deferred unmap-acks. No-op without a message placer or outstanding request. A
        // guest kick the wait drained is served by another pass.
        #[cfg(target_os = "linux")]
        let more = {
          let driven = drive_dax_acks(admitted, bridge).await;
          if let Some(reason) = driven.end {
            return Some(reason);
          }
          more || driven.kicked
        };
        report_dax_counts(admitted, bridge, counters);
        if more {
          futures::yield_now().await;
        } else {
          return None;
        }
      }
      Err(refusal) => return Some(end_of(refusal)),
    }
  }
}

/// One service pass and, when a mutation's reply waits for the owner's barrier, the barrier and the
/// completion; whether more work is pending. With a DAX placer, the device's backed DAX takes a step first
/// (`AdmittedDevice::dax_turn`); a busy step serves no chain this pass.
fn one_pass<S: VmmSeam, B: BridgeAccess>(
  admitted: &mut AdmittedDevice<S>,
  bridge: &mut B,
  counters: &mut LoopCounters,
) -> Result<bool, ServeError> {
  {
    let slice = bridge.copy_slice_bytes();
    let turned = bridge
      .with_bridge(|b, registry| admitted.dax_turn(b, registry, slice))
      .map_err(ServeError::Authority)??;
    if turned.barrier_owed {
      // A request the step served changed what survives a restart, or its ingest wrote: publish before the guest
      // learns its reply, as a mutation's pass does.
      let captured = bridge.barrier();
      bridge
        .with_bridge(|b, registry| admitted.complete_barrier(b, registry, captured))
        .map_err(ServeError::Authority)??;
    }
    if turned.busy {
      counters.passes = counters.passes.saturating_add(1);
      slates_rt::registry::with_current(|ctx| ctx.note_activity());
      return Ok(true);
    }
  }
  let pass = bridge
    .with_bridge(|b, registry| admitted.service(b, registry))
    .map_err(ServeError::Authority)??;
  counters.passes = counters.passes.saturating_add(1);
  if pass.barrier_owed {
    // A mutation's reply waits for the owner's barrier: publish, then let the guest learn the reply (or
    // `EIO`, when the publication did not capture the volume) before any later chain is served.
    let captured = bridge.barrier();
    bridge
      .with_bridge(|b, registry| admitted.complete_barrier(b, registry, captured))
      .map_err(ServeError::Authority)??;
  }
  // A guest's requests are client activity: the shard spins out its idle window after a pass, so the
  // guest's next kick lands in the spin rather than waking a parked shard (§4.7).
  slates_rt::registry::with_current(|ctx| ctx.note_activity());
  Ok(pass.more_pending)
}

/// What awaiting a parked map's acknowledgements came to: why the loop must end, if it must, and whether the wait
/// drained a guest kick, which another pass then serves.
#[cfg(target_os = "linux")]
struct AcksDriven {
  end: Option<EndReason>,
  kicked: bool,
}

/// What ended a wait on a map's acknowledgement.
#[cfg(target_os = "linux")]
enum AckWoke {
  /// The back-end-request channel is readable: an acknowledgement arrived.
  Ack,
  /// The doorbell is readable: a control message from the front end, or a guest kick.
  Doorbell,
}

/// Waits until the back-end-request channel `ack` or, where the device has one, the doorbell is readable. Each
/// readiness answers only for its own descriptor (A-149), so an acknowledgement is never taken for a control message.
#[cfg(target_os = "linux")]
async fn ack_or_doorbell(ack: i32, doorbell: Option<i32>) -> Result<AckWoke, RtError> {
  let mut ack = std::pin::pin!(readable(ack));
  let mut doorbell = std::pin::pin!(doorbell.map(readable));
  std::future::poll_fn(|cx| {
    if let Poll::Ready(ready) = ack.as_mut().poll(cx) {
      return Poll::Ready(ready.map(|()| AckWoke::Ack));
    }
    match doorbell.as_mut().as_pin_mut() {
      Some(doorbell) => doorbell
        .poll(cx)
        .map(|ready| ready.map(|()| AckWoke::Doorbell)),
      None => Poll::Pending,
    }
  })
  .await
}

/// Completes a parked message-based DAX map (§4.6): awaits the front end's reply-acks on the back-end-request channel,
/// one a piece of the window, then answers the guest's SETUPMAPPING; and settles any deferred unmap-acks. While an
/// acknowledgement is owed it serves the front end's control messages too: QEMU answers each piece's map and then
/// commits it, sending `SET_MEM_TABLE` and awaiting the answer before it reads the next piece, so a window of two pieces
/// waited forever on a loop that served nothing else (A-194, found by QEMU 11.1's guest tests). A kick drained
/// meanwhile is reported, so the loop serves it. `end` is why the loop must end, if it must: the readiness was lost, the
/// front end closed, or the attachment's volume is gone. The fd-sharing path has no acks, so this is Linux only; a
/// device with no outstanding DAX request does nothing.
#[cfg(target_os = "linux")]
async fn drive_dax_acks<S: VmmSeam, B: BridgeAccess>(
  admitted: &mut AdmittedDevice<S>,
  bridge: &mut B,
) -> AcksDriven {
  let mut kicked = false;
  while admitted.dax_map_ack_owed() {
    let Some(raw) = admitted.dax_ack_raw_fd() else {
      break;
    };
    let doorbell = match admitted.doorbell() {
      Doorbell::Descriptor(doorbell) => Some(doorbell),
      Doorbell::InProcess => None,
    };
    match ack_or_doorbell(raw, doorbell).await {
      Err(error) => {
        return AcksDriven {
          end: Some(EndReason::DoorbellLost(error)),
          kicked,
        };
      }
      Ok(AckWoke::Doorbell) => {
        match admitted.drain_doorbell() {
          Ok(Drained::HungUp) => {
            return AcksDriven {
              end: Some(EndReason::DoorbellHungUp),
              kicked,
            };
          }
          Ok(Drained::Kicked) => kicked = true,
          Ok(Drained::Nothing) => {}
          Err(e) => {
            return AcksDriven {
              end: Some(EndReason::Faulted(ServeError::Seam(e))),
              kicked,
            };
          }
        }
        continue;
      }
      Ok(AckWoke::Ack) => {}
    }
    match bridge.with_bridge(|_, registry| admitted.complete_dax_map(registry)) {
      Ok(Ok(_)) => {}
      Ok(Err(e)) => {
        return AcksDriven {
          end: Some(EndReason::Faulted(e)),
          kicked,
        };
      }
      Err(gone) => {
        return AcksDriven {
          end: Some(EndReason::VolumeGone(gone)),
          kicked,
        };
      }
    }
  }
  // Opportunistically settle any arrived unmap-acks without blocking, so the outstanding ring stays short.
  if admitted.dax_outstanding() {
    match bridge.with_bridge(|_, registry| admitted.complete_dax_map(registry)) {
      Ok(Ok(_)) | Err(_) => {}
      Ok(Err(e)) => {
        return AcksDriven {
          end: Some(EndReason::Faulted(e)),
          kicked,
        };
      }
    }
  }
  AcksDriven { end: None, kicked }
}

/// The ingest the guest's stores through its maps take before the device ends (§4.6, A-163): every backing with ranges
/// asked writable is compared with the volume a slice at a time, yielding between slices, and what it wrote is published
/// before the terminal step lets the backings go. It waits while the owner is fenced, as every pass does, and the
/// guest's stores are lost, counted, only when the attachment's authority or its volume is gone.
async fn drive_end_ingest<S: VmmSeam, B: BridgeAccess>(
  admitted: &mut AdmittedDevice<S>,
  bridge: &mut B,
) {
  if !bridge
    .with_bridge(|b, _| admitted.begin_end_ingest(b))
    .unwrap_or(false)
  {
    return;
  }
  loop {
    if let Some(wait) = bridge.fenced() {
      if futures::sleep(wait).await.is_err() {
        admitted.abort_end_ingest();
        return;
      }
      continue;
    }
    let slice = bridge.copy_slice_bytes();
    match bridge.with_bridge(|b, registry| admitted.end_ingest_step(b, registry, slice)) {
      Ok(Ok(None)) => futures::yield_now().await,
      Ok(Ok(Some(wrote))) => {
        if wrote {
          // A refused publication is counted by the owner's barrier; the stores are in the volume either way.
          let _captured = bridge.barrier();
        }
        return;
      }
      Ok(Err(_)) | Err(_) => {
        admitted.abort_end_ingest();
        return;
      }
    }
  }
}

/// What the loop counted, reported in [`ServeEnd`].
#[derive(Clone, Copy, Debug, Default)]
struct LoopCounters {
  wakes: u64,
  passes: u64,
  fenced_waits: u64,
  /// The device's backed DAX counts as the loop last added them to the owner's status counters.
  dax_reported: crate::backed::BackedCounts,
}

/// Adds what the device's backed DAX did since the loop's last report to the owner's status counters (A-168): after
/// every pass, and once more after the terminal step, so its end ingest and its releases count too.
fn report_dax_counts<S: VmmSeam, B: BridgeAccess>(
  admitted: &AdmittedDevice<S>,
  bridge: &mut B,
  counters: &mut LoopCounters,
) {
  let now = admitted.device().dax_backed_counts();
  for ((name, total), (_, reported)) in now.named().into_iter().zip(counters.dax_reported.named()) {
    let moved = total.saturating_sub(reported);
    if moved > 0 {
      bridge.count(name, moved);
    }
  }
  counters.dax_reported = now;
}

/// One wait on the doorbell and the service it triggers; `Some(reason)` when the loop must end.
async fn serve_round<S: VmmSeam, B: BridgeAccess>(
  id: DeviceId,
  admitted: &mut AdmittedDevice<S>,
  bridge: &mut B,
  counters: &mut LoopCounters,
) -> Option<EndReason> {
  if revoke_requested(id) {
    return Some(EndReason::Revoked);
  }
  RegisterWaker { id }.await;
  // The attachment's view moved (§4.6 "revoke on advance": a snapshot reader's `advance` kicked the loop): mark every
  // backing stale now, so each is refilled from the new view before the guest sees anything more, without waiting for
  // the guest's next doorbell kick, then serve. A loop with no kick falls straight through to its doorbell wait.
  if take_advance_requested(id) {
    // If the attachment's authority has just lapsed, the pass below meets the same lapse and ends the loop on it, so
    // the refusal here is folded into that pass, not lost.
    let _advanced = bridge.with_bridge(|b, registry| admitted.view_advanced(b, registry));
    return serve_until_idle(id, admitted, bridge, counters).await;
  }
  let raw = match admitted.doorbell() {
    Doorbell::InProcess => return Some(EndReason::NoDoorbell),
    Doorbell::Descriptor(raw) => raw,
  };
  // While a reply waits for the volume's FUSE mounts, the round waits on the oldest's too, and on its bound (AC-3.3).
  let told = admitted.told_oldest().map(|oldest| {
    let bound = bridge.told_bound_ns();
    let left = oldest.since_ns.map_or(bound, |since| {
      bound.saturating_sub(futures::now_ns().saturating_sub(since))
    });
    (bridge.told(oldest.seq), left)
  });
  match doorbell_request_or_told(id, raw, told).await {
    Err(error) => return Some(EndReason::DoorbellLost(error)),
    // The shard asked something of the loop while it waited: the next round takes it at its top (a revoke ends
    // the loop, a kick marks its backings stale), with no guest kick needed.
    Ok(Woke::Request) => return None,
    // A held reply's mounts were told, or its bound passed: it is published, and the next round waits again.
    Ok(Woke::Told) => return settle_told(admitted, bridge).err().map(end_of),
    Ok(Woke::Doorbell) => {}
  }
  counters.wakes = counters.wakes.saturating_add(1);
  if revoke_requested(id) {
    return Some(EndReason::Revoked);
  }
  match admitted.drain_doorbell() {
    Ok(Drained::HungUp) => return Some(EndReason::DoorbellHungUp),
    Ok(Drained::Kicked | Drained::Nothing) => {}
    Err(e) => return Some(EndReason::Faulted(ServeError::Seam(e))),
  }
  serve_until_idle(id, admitted, bridge, counters).await
}

/// The perpetual device loop for `admitted` as loop `id` (from [`register`]), reaching the bridge
/// through `bridge`. Ends on a revoke request, a doorbell hangup, a fault, or a lost driver — and
/// runs the owned terminal step before it returns.
pub async fn serve_loop<S: VmmSeam, B: BridgeAccess>(
  id: DeviceId,
  mut admitted: AdmittedDevice<S>,
  mut bridge: B,
) -> ServeEnd {
  let mut counters = LoopCounters::default();
  // This loop settles the replies that wait for the volume's FUSE mounts, after every pass and while idle (AC-3.3).
  admitted.hold_replies_for_told();
  let why = loop {
    if let Some(reason) = serve_round(id, &mut admitted, &mut bridge, &mut counters).await {
      break reason;
    }
  };
  let abandoned = admitted.device().told_held();
  if abandoned > 0 {
    bridge.count(TOLD_ABANDONED, u64::try_from(abandoned).unwrap_or(u64::MAX));
  }
  // The guest's stores through its DAX maps reach the volume before anything of the attachment is let go.
  drive_end_ingest(&mut admitted, &mut bridge).await;
  // The terminal step always runs to its end (AUD-29-70): through the bridge when the volume is there, else
  // through the registry alone — the volume's references went with it — and the store's backings alone.
  let reclaimed = match bridge.with_bridge(|b, registry| admitted.reclaim(b, registry)) {
    Ok(reclaimed) => reclaimed,
    Err(gone) => {
      let _released = bridge.with_backings(|backings| admitted.release_backings(backings));
      bridge
        .with_registry(|registry| admitted.abandon(registry))
        .unwrap_or(Err(ReclaimError::Authority(gone)))
    }
  };
  report_dax_counts(&admitted, &mut bridge, &mut counters);
  unregister(id);
  ServeEnd {
    why,
    reclaimed,
    wakes: counters.wakes,
    passes: counters.passes,
    fenced_waits: counters.fenced_waits,
  }
}

#[cfg(test)]
mod tests {
  use super::{DeviceId, kick, register, request_revoke, revoke_requested, take_advance_requested};

  /// §4.6 — a kick for a moved view marks a device loop once, read-and-cleared, and is a separate flag from a revoke.
  /// Do: register a loop; kick it; take the mark twice; then revoke it. Expect: the kick finds the loop and marks it;
  /// the first take is true and the second false (one-shot, cleared); a kick does not revoke; revoke is its own flag;
  /// and a kick of an unknown loop is false.
  #[test]
  fn a_kick_marks_the_loop_once_and_is_distinct_from_revoke() {
    // A generous bound: this thread-local control map may hold loops from other tests sharing the thread.
    let id = register(1024).unwrap();
    assert!(
      !take_advance_requested(id),
      "nothing is marked before a kick"
    );
    assert!(kick(id), "the kick finds the registered loop");
    assert!(take_advance_requested(id), "the kick marked the loop");
    assert!(
      !take_advance_requested(id),
      "the mark is one-shot — the take cleared it, so the loop takes it once per kick"
    );
    assert!(!revoke_requested(id), "a kick does not revoke");
    assert!(request_revoke(id), "the same loop can be revoked");
    assert!(
      revoke_requested(id),
      "revoke is its own flag, independent of the kick's"
    );
    assert!(
      !kick(DeviceId(u64::MAX)),
      "a kick of a loop that does not run here is false"
    );
  }
}
