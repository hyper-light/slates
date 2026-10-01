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

/// A loop's control entry: the waker to interrupt it with, and whether revocation was asked for.
struct LoopControl {
  waker: Option<Waker>,
  revoke: bool,
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

/// A future that records the loop's current waker in its control entry, so a revoke request can
/// interrupt the loop's wait on the doorbell.
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
      Ok(true) => futures::yield_now().await,
      Ok(false) => return None,
      Err(ServeError::Revoked) => return Some(EndReason::Revoked),
      Err(ServeError::Authority(VfsError::NotFound)) => {
        return Some(EndReason::VolumeGone(VfsError::NotFound));
      }
      Err(e) => return Some(EndReason::Faulted(e)),
    }
  }
}

/// One service pass and, when a mutation's reply waits for the owner's barrier, the barrier and the
/// completion; whether more work is pending.
fn one_pass<S: VmmSeam, B: BridgeAccess>(
  admitted: &mut AdmittedDevice<S>,
  bridge: &mut B,
  counters: &mut LoopCounters,
) -> Result<bool, ServeError> {
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

/// What the loop counted, reported in [`ServeEnd`].
#[derive(Clone, Copy, Debug, Default)]
struct LoopCounters {
  wakes: u64,
  passes: u64,
  fenced_waits: u64,
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
  let raw = match admitted.doorbell() {
    Doorbell::InProcess => return Some(EndReason::NoDoorbell),
    Doorbell::Descriptor(raw) => raw,
  };
  if let Err(error) = readable(raw).await {
    return Some(EndReason::DoorbellLost(error));
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
  let reclaimed = bridge
    .with_bridge(|b, registry| admitted.reclaim(b, registry))
    .unwrap_or_else(|gone| Err(ReclaimError::Authority(gone)));
  unregister(id);
  ServeEnd {
    why,
    reclaimed,
    wakes: counters.wakes,
    passes: counters.passes,
    fenced_waits: counters.fenced_waits,
  }
}
