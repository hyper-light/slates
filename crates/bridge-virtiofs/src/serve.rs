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
}

/// Runs service passes until the rings are idle, yielding between passes.
async fn serve_until_idle<S: VmmSeam, B: BridgeAccess>(
  admitted: &mut AdmittedDevice<S>,
  bridge: &mut B,
  passes: &mut u64,
) -> Result<(), ServeError> {
  loop {
    let pass = bridge
      .with_bridge(|b, registry| admitted.service(b, registry))
      .map_err(ServeError::Authority)??;
    *passes = passes.saturating_add(1);
    if !pass.more_pending {
      return Ok(());
    }
    futures::yield_now().await;
  }
}

/// One wait on the doorbell and the service it triggers; `Some(reason)` when the loop must end.
async fn serve_round<S: VmmSeam, B: BridgeAccess>(
  id: DeviceId,
  admitted: &mut AdmittedDevice<S>,
  bridge: &mut B,
  counters: &mut (u64, u64),
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
  counters.0 = counters.0.saturating_add(1);
  if revoke_requested(id) {
    return Some(EndReason::Revoked);
  }
  match admitted.drain_doorbell() {
    Ok(Drained::HungUp) => return Some(EndReason::DoorbellHungUp),
    Ok(Drained::Kicked | Drained::Nothing) => {}
    Err(e) => return Some(EndReason::Faulted(ServeError::Seam(e))),
  }
  match serve_until_idle(admitted, bridge, &mut counters.1).await {
    Ok(()) => None,
    Err(ServeError::Revoked) => Some(EndReason::Revoked),
    Err(ServeError::Authority(VfsError::NotFound)) => {
      Some(EndReason::VolumeGone(VfsError::NotFound))
    }
    Err(e) => Some(EndReason::Faulted(e)),
  }
}

/// The perpetual device loop for `admitted` as loop `id` (from [`register`]), reaching the bridge
/// through `bridge`. Ends on a revoke request, a doorbell hangup, a fault, or a lost driver — and
/// runs the owned terminal step before it returns.
pub async fn serve_loop<S: VmmSeam, B: BridgeAccess>(
  id: DeviceId,
  mut admitted: AdmittedDevice<S>,
  mut bridge: B,
) -> ServeEnd {
  let mut counters = (0u64, 0u64);
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
    wakes: counters.0,
    passes: counters.1,
  }
}
