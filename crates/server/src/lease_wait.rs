//! Lease reads wait for their confirmation (§4.8 "Leases and reads"; AUD-08). A verb that serves an object's
//! **latest state** on its owner shard while the owner lease is unconfirmed is **parked**, not refused: it runs
//! the moment the lease confirms, and is refused `LeaseUnconfirmed` only if the lease bound passes first.
//!
//! **Why wait.** An unconfirmed lease is usually a confirmation in flight: right after a volume's creation, after
//! a configuration change the holders are still answering, after a starved period. A refusal there made every
//! client write its own retry loop, and a client that did not (`slates mount` right after `volume create`) failed
//! on a busy machine: the three-process CLI deployment refused its first mount `LeaseUnconfirmed` in about one run
//! of ten under load (2026-10-04). Waiting is how a lease read is served elsewhere: a Raft leader serves a read
//! only after a heartbeat round confirms it still leads (Ongaro, "Consensus: Bridging Theory and Practice",
//! §6.4), and etcd's `ReadIndex` blocks the read until that round completes. The daemon, not each client, holds
//! the wait, because the daemon learns the confirmation the moment it is fanned here, and a cut-off owner's
//! clients wait once each instead of retrying in a herd.
//!
//! **What holds.** Nothing of a parked verb runs and nothing is recorded until it is resolved: a waiter carries
//! the request's completion key, its principal and its body, and on resolution the verb runs through
//! `verbs::dispatch` under its key and its reply is recorded as its completion in one durable step, then
//! delivered as a deferred acceptance's is (`merge_service::resolve_accepted`). A retry of a parked request joins
//! it ([`join`]). The list is bounded by the shard's client credit, the forwards that can be in flight
//! (`clients_per_shard × slots`, the bound `forwarded_rings` keeps); a verb past it is refused at once, counted.
//! A waiter's deadline is the lease bound from its parking: by then a healthy owner's holders have answered at
//! least once, so an owner still unconfirmed is cut off, and the refusal says so.

use std::collections::BTreeSet;

use slates_db::catalog::Principal;
use slates_db::register::ObjectId;
use slates_ipc::protocol::{Refusal, ReplyBody, RequestBody};
use slates_wire::request::RequestId;

use crate::merge_service::ReplyRoute;
use crate::state::{self, ShardState};

/// The status refusal count of a verb parked for its lease.
const PARKED: &str = "lease.wait.parked";
/// The status refusal count of a parked verb served once its lease confirmed (the non-vacuity count).
const SERVED: &str = "lease.wait.served";
/// The status refusal count of a parked verb refused at its deadline: its lease never confirmed.
const EXPIRED: &str = "lease.wait.expired";
/// The status refusal count of a verb refused at once because the waiter list was full.
const FULL: &str = "lease.wait.full";
/// The status refusal count of a resolved verb whose completion could not be made durable (the client's retry
/// runs it again).
const UNRECORDED: &str = "lease.wait.unrecorded";
/// The status refusal count of a resolved reply the client's shard refused to admit (its retry meets the
/// completion record).
const UNDELIVERED: &str = "lease.wait.undelivered";
/// The status refusal count of a deadline timer the shard's arena refused (waiters are still resolved at the next
/// lease fan).
const TIMER_REFUSED: &str = "lease.wait.timer_refused";

/// One parked verb.
pub(crate) struct LeaseWaiter {
  route: Option<ReplyRoute>,
  origin: u64,
  id: RequestId,
  client_id: u32,
  principal: Principal,
  body: RequestBody,
  object: ObjectId,
  deadline_ns: u64,
}

/// A shard's parked verbs.
#[derive(Default)]
pub(crate) struct LeaseWaiters {
  waiting: Vec<LeaseWaiter>,
  /// The completion keys parked, so a retry joins rather than parks twice.
  by_request: BTreeSet<(u64, u32, u32)>,
  /// Whether this shard's deadline timer task is running.
  timer_armed: bool,
}

impl LeaseWaiters {
  /// The verbs parked now.
  pub(crate) fn len(&self) -> usize {
    self.waiting.len()
  }
}

fn key(origin: u64, id: RequestId) -> (u64, u32, u32) {
  (origin, id.client, id.sequence)
}

/// The most verbs a shard parks: its client credit (`clients_per_shard × slots`).
fn bound(state: &ShardState) -> usize {
  state
    .config
    .clients_per_shard
    .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(1))
}

/// Parks the running verb (`body` from `client_id` as `principal`) until `object`'s lease confirms, marking it
/// deferred so `run_recorded` records nothing and writes no reply. Returns `false` when it cannot be parked — no
/// recorded request is running, or the list is full — and the caller refuses as before.
pub(crate) fn park(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
  object: ObjectId,
) -> bool {
  let Some((origin, id)) = state.current_request else {
    return false;
  };
  if state.lease_waiters.waiting.len() >= bound(state) {
    *state.refusals.entry(FULL).or_insert(0) += 1;
    return false;
  }
  // The lease's own clock (suspend-inclusive), so a paused owner's waiters expire as its lease does.
  let deadline_ns =
    slates_machine::clock::monotonic_ns().saturating_add(crate::lease::lease_bound_ns());
  let route = state.reply_route.take();
  state.lease_waiters.waiting.push(LeaseWaiter {
    route,
    origin,
    id,
    client_id,
    principal: principal.clone(),
    body,
    object,
    deadline_ns,
  });
  state.lease_waiters.by_request.insert(key(origin, id));
  state.acceptance_deferred = true;
  *state.refusals.entry(PARKED).or_insert(0) += 1;
  arm_timer(state);
  true
}

/// A retry of a parked request joins it: it is answered when the parked verb resolves. Returns whether the
/// request was parked.
pub(crate) fn join(state: &mut ShardState, origin: u64, id: RequestId) -> bool {
  if !state.lease_waiters.by_request.contains(&key(origin, id)) {
    return false;
  }
  // The newest route wins: a retry on a reconnected client is answered where it now waits.
  if let Some(route) = state.reply_route.take()
    && let Some(waiter) = state
      .lease_waiters
      .waiting
      .iter_mut()
      .find(|waiter| waiter.origin == origin && waiter.id == id)
  {
    waiter.route = Some(route);
  }
  true
}

/// Resolves every parked verb whose lease now confirms (it runs) or whose deadline has passed (it is refused
/// `LeaseUnconfirmed`); the rest stay parked. Called when lease evidence reaches this shard and at each deadline.
pub(crate) fn resolve(state: &mut ShardState) {
  if state.lease_waiters.waiting.is_empty() {
    return;
  }
  let now = slates_machine::clock::monotonic_ns();
  let waiting = std::mem::take(&mut state.lease_waiters.waiting);
  let mut still = Vec::with_capacity(waiting.len());
  for waiter in waiting {
    let confirmed = crate::verbs::lease_verdict(state, waiter.object)
      .refusal_count_name()
      .is_none();
    if !confirmed && now < waiter.deadline_ns {
      still.push(waiter);
      continue;
    }
    state
      .lease_waiters
      .by_request
      .remove(&key(waiter.origin, waiter.id));
    run(state, waiter, confirmed);
  }
  // A verb re-parked while resolving (its lease lapsed again) was pushed meanwhile; both are kept.
  still.append(&mut state.lease_waiters.waiting);
  state.lease_waiters.waiting = still;
}

/// Runs one resolved waiter: the verb under its completion key when its lease `confirmed`, else the refusal; the
/// reply recorded as its completion in one durable step and delivered where the request waits.
fn run(state: &mut ShardState, waiter: LeaseWaiter, confirmed: bool) {
  let LeaseWaiter {
    route,
    origin,
    id,
    client_id,
    principal,
    body,
    ..
  } = waiter;
  state.db.begin();
  state.current_request = Some((origin, id));
  state.reply_route = route;
  let reply = if confirmed {
    crate::verbs::dispatch(state, client_id, &principal, body)
  } else {
    *state.refusals.entry(EXPIRED).or_insert(0) += 1;
    crate::verbs::refused(Refusal::LeaseUnconfirmed {
      version: state.fleet.configuration().version,
    })
  };
  state.current_request = None;
  let route = state.reply_route.take().or(route);
  if std::mem::take(&mut state.acceptance_deferred) {
    // Deferred again (parked anew, or a deferral of the verb's own): that deferral carries the route and
    // answers; the effects so far commit with no completion, as `run_recorded` does.
    if state.db.commit(&mut state.segment).is_err() {
      crate::verbs::reconcile_unpublished_effects(state);
    }
    return;
  }
  let recorded = crate::verbs::record_completion(state, origin, id, reply);
  if state.db.commit(&mut state.segment).is_err() {
    crate::verbs::reconcile_unpublished_effects(state);
    *state.refusals.entry(UNRECORDED).or_insert(0) += 1;
    return;
  }
  if confirmed {
    *state.refusals.entry(SERVED).or_insert(0) += 1;
  }
  let Some(route) = route else {
    // A verb forwarded from another node: its exchange polls the completion record.
    return;
  };
  let delivery = slates_rt::task::SpawnRequest::new(
    Box::pin(async move {
      crate::state::deliver(route.client_index, route.request, recorded, true);
    }),
    None,
  );
  if slates_rt::registry::send_control(
    route.shard,
    slates_rt::control::Control::Spawn(Box::new(delivery)),
  )
  .is_err()
  {
    *state.refusals.entry(UNDELIVERED).or_insert(0) += 1;
  }
}

/// Arms this shard's deadline timer if it is not running: one task that sleeps until the earliest deadline,
/// resolves, and goes on while verbs are parked (one per shard, so bounded).
fn arm_timer(state: &mut ShardState) {
  if state.lease_waiters.timer_armed {
    return;
  }
  match slates_rt::futures::spawn(deadline_timer()) {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
      state.lease_waiters.timer_armed = true;
    }
    Err(_) => {
      *state.refusals.entry(TIMER_REFUSED).or_insert(0) += 1;
    }
  }
}

async fn deadline_timer() {
  loop {
    let wait = state::with_state(|s| {
      let now = slates_machine::clock::monotonic_ns();
      let earliest = s
        .lease_waiters
        .waiting
        .iter()
        .map(|waiter| waiter.deadline_ns)
        .min();
      match earliest {
        Some(deadline) => Some(deadline.saturating_sub(now)),
        None => {
          s.lease_waiters.timer_armed = false;
          None
        }
      }
    })
    .flatten();
    let Some(wait) = wait else {
      return;
    };
    // A refused sleep (the timer wheel full) still resolves now; the next turn sleeps again.
    let _ = slates_rt::futures::sleep(wait.max(1)).await;
    let _ = state::with_state(resolve);
  }
}

/// The verbs parked on this shard, for a test's observation.
pub(crate) fn parked(state: &ShardState) -> usize {
  state.lease_waiters.len()
}

/// A placeholder for a parked verb's reply: never written (`run_recorded` drops a deferred verb's reply that is
/// not a refusal), never recorded.
pub(crate) fn parked_reply() -> ReplyBody {
  ReplyBody::Edited
}
