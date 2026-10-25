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

/// The identity of one device loop on this shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeviceId(u64);

/// A loop's control entry: the waker to interrupt it with, whether revocation was asked for, and whether a daemon
/// recall asked the loop to drain its pending DAX revokes now (a *kick*, distinct from a revoke — the loop drains
/// and keeps serving).
struct LoopControl {
  waker: Option<Waker>,
  revoke: bool,
  drain: bool,
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
        drain: false,
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

/// Kicks the loop `id` to drain its pending DAX revokes on its next turn, without a guest doorbell kick and without
/// ending it (§4.6: a daemon recall — e.g. a `snapshot`'s writable-window teardown — marks the inode revoke-pending
/// and kicks the owning device loop so the munmap happens promptly, not on the guest's next kick). `false` when no
/// such loop runs on this shard. Idempotent: a second kick before the loop drains is folded into the one flag.
pub fn kick(id: DeviceId) -> bool {
  CONTROL.with(|control| {
    let mut control = control.borrow_mut();
    let Some(entry) = control.get_mut(&id.0) else {
      return false;
    };
    entry.drain = true;
    if let Some(waker) = entry.waker.take() {
      waker.wake();
    }
    true
  })
}

/// Reads and clears the drain request for loop `id` (a kick); whether one was pending.
fn take_drain_requested(id: DeviceId) -> bool {
  CONTROL.with(|control| {
    control
      .borrow_mut()
      .get_mut(&id.0)
      .is_some_and(|e| std::mem::take(&mut e.drain))
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

/// Whether the shard has asked something of loop `id` that it has not taken yet: a revoke, or a kick to drain.
fn request_pending(id: DeviceId) -> bool {
  CONTROL.with(|control| {
    control
      .borrow()
      .get(&id.0)
      .is_some_and(|e| e.revoke || e.drain)
  })
}

/// What ended a loop's wait on its doorbell.
enum Woke {
  /// The guest kicked the doorbell (or hung it up: the drain tells which).
  Doorbell,
  /// The shard asked for a revoke or a drain ([`request_revoke`], [`kick`]).
  Request,
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
    match one_pass(admitted, bridge, counters) {
      Ok(more) => {
        // A message-based SETUPMAPPING may have parked for the front end's map-ack (§4.6); await it and answer
        // the guest, and settle any deferred unmap-acks. No-op without a message placer or outstanding request.
        #[cfg(target_os = "linux")]
        if let Some(reason) = drive_dax_acks(admitted, bridge).await {
          return Some(reason);
        }
        #[cfg(target_os = "linux")]
        report_dax_counts(admitted, bridge, counters);
        if more {
          futures::yield_now().await;
        } else {
          return None;
        }
      }
      Err(ServeError::Revoked) => return Some(EndReason::Revoked),
      Err(ServeError::Authority(VfsError::NotFound)) => {
        return Some(EndReason::VolumeGone(VfsError::NotFound));
      }
      Err(e) => return Some(EndReason::Faulted(e)),
    }
  }
}

/// One service pass and, when a mutation's reply waits for the owner's barrier, the barrier and the
/// completion; whether more work is pending. With the message placer (Linux), its backed DAX takes a step first
/// (`AdmittedDevice::dax_turn`); a busy step serves no chain this pass.
fn one_pass<S: VmmSeam, B: BridgeAccess>(
  admitted: &mut AdmittedDevice<S>,
  bridge: &mut B,
  counters: &mut LoopCounters,
) -> Result<bool, ServeError> {
  #[cfg(target_os = "linux")]
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

/// Completes a parked message-based DAX map (§4.6): await the front end's reply-ack on the back-end-request
/// channel, then answer the guest's SETUPMAPPING; and settle any deferred unmap-acks (unpinning their blocks).
/// `Some(reason)` when the loop must end — the channel's readiness was lost, the front end closed, or the
/// attachment's volume is gone. The fd-sharing path has no acks, so this is Linux only; a device with no
/// outstanding DAX request does nothing.
#[cfg(target_os = "linux")]
async fn drive_dax_acks<S: VmmSeam, B: BridgeAccess>(
  admitted: &mut AdmittedDevice<S>,
  bridge: &mut B,
) -> Option<EndReason> {
  while admitted.dax_map_ack_owed() {
    let Some(raw) = admitted.dax_ack_raw_fd() else {
      break;
    };
    if let Err(error) = readable(raw).await {
      return Some(EndReason::DoorbellLost(error));
    }
    match bridge.with_bridge(|_, registry| admitted.complete_dax_map(registry)) {
      Ok(Ok(_)) => {}
      Ok(Err(e)) => return Some(EndReason::Faulted(e)),
      Err(gone) => return Some(EndReason::VolumeGone(gone)),
    }
  }
  // Opportunistically settle any arrived unmap-acks without blocking, so the outstanding ring stays short.
  if admitted.dax_outstanding() {
    match bridge.with_bridge(|_, registry| admitted.complete_dax_map(registry)) {
      Ok(Ok(_)) | Err(_) => {}
      Ok(Err(e)) => return Some(EndReason::Faulted(e)),
    }
  }
  None
}

/// The ingest the guest's stores through its maps take before the device ends (§4.6, A-163): every backing with ranges
/// asked writable is compared with the volume a slice at a time, yielding between slices, and what it wrote is published
/// before the terminal step lets the backings go. It waits while the owner is fenced, as every pass does, and the
/// guest's stores are lost, counted, only when the attachment's authority or its volume is gone. Linux only.
#[cfg(target_os = "linux")]
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
  #[cfg(target_os = "linux")]
  dax_reported: crate::backed::BackedCounts,
}

/// Adds what the device's backed DAX did since the loop's last report to the owner's status counters (A-168): after
/// every pass, and once more after the terminal step, so its end ingest and its releases count too.
#[cfg(target_os = "linux")]
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
  // A daemon recall kicked the loop (§4.6; e.g. `snapshot` recalling a volume's writable DAX windows): drain the
  // pending revokes now — munmap the guest windows the recall marked — without waiting for the guest's next
  // doorbell kick, then resume. Additive to the doorbell path below (unchanged): a loop with no kick falls straight
  // through to its doorbell wait.
  if take_drain_requested(id) {
    // An `advance` kick (§4.6 "revoke on advance") rides the same flag: before the drain, re-validate this device's
    // windows against its now-current view and tear any it no longer backs (the moved reader's stale windows), so
    // the guest re-maps against the new view. A device whose view did not change finds all windows current, so a
    // volume-wide kick is self-scoping. Best-effort hardening: if the attachment's authority has just lapsed, the
    // pending drain and the terminal reclaim still cover teardown, and the pass below surfaces the lapse — so the
    // result is folded into that pass, not lost.
    let _revalidated = bridge.with_bridge(|b, registry| admitted.revalidate_windows(b, registry));
    return serve_until_idle(id, admitted, bridge, counters).await;
  }
  let raw = match admitted.doorbell() {
    Doorbell::InProcess => return Some(EndReason::NoDoorbell),
    Doorbell::Descriptor(raw) => raw,
  };
  match doorbell_or_request(id, raw).await {
    Err(error) => return Some(EndReason::DoorbellLost(error)),
    // The shard asked something of the loop while it waited: the next round takes it at its top (a revoke ends
    // the loop, a kick drains its pending DAX revokes), with no guest kick needed.
    Ok(Woke::Request) => return None,
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
  let why = loop {
    if let Some(reason) = serve_round(id, &mut admitted, &mut bridge, &mut counters).await {
      break reason;
    }
  };
  // The guest's stores through its DAX maps reach the volume before anything of the attachment is let go.
  #[cfg(target_os = "linux")]
  drive_end_ingest(&mut admitted, &mut bridge).await;
  // The terminal step always runs to its end (AUD-29-70): through the bridge when the volume is there, else
  // through the registry alone — the volume's references went with it — and the store's backings alone.
  let reclaimed = match bridge.with_bridge(|b, registry| admitted.reclaim(b, registry)) {
    Ok(reclaimed) => reclaimed,
    Err(gone) => {
      #[cfg(target_os = "linux")]
      let _released = bridge.with_backings(|backings| admitted.release_backings(backings));
      bridge
        .with_registry(|registry| admitted.abandon(registry))
        .unwrap_or(Err(ReclaimError::Authority(gone)))
    }
  };
  #[cfg(target_os = "linux")]
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
  use super::{DeviceId, kick, register, request_revoke, revoke_requested, take_drain_requested};

  /// §4.6 — a daemon recall *kick* marks a device loop to drain its pending DAX revokes once, read-and-cleared, and
  /// is a separate flag from a revoke. Do: register a loop; kick it; read the drain request twice; then revoke it.
  /// Expect: the kick finds the loop and marks it; the first take is true and the second false (one-shot, cleared);
  /// a kick does not revoke; revoke is its own flag; and a kick of an unknown loop is false.
  #[test]
  fn a_kick_marks_the_loop_to_drain_once_and_is_distinct_from_revoke() {
    // A generous bound: this thread-local control map may hold loops from other tests sharing the thread.
    let id = register(1024).unwrap();
    assert!(
      !take_drain_requested(id),
      "no drain is pending before a kick"
    );
    assert!(kick(id), "the kick finds the registered loop");
    assert!(
      take_drain_requested(id),
      "the kick marked the loop to drain"
    );
    assert!(
      !take_drain_requested(id),
      "the drain request is one-shot — the take cleared it, so the loop drains once per kick"
    );
    assert!(!revoke_requested(id), "a kick drains; it does not revoke");
    assert!(request_revoke(id), "the same loop can be revoked");
    assert!(
      revoke_requested(id),
      "revoke is its own flag, independent of the drain flag"
    );
    assert!(
      !kick(DeviceId(u64::MAX)),
      "a kick of a loop that does not run here is false"
    );
  }
}
