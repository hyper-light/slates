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
/// The status refusal count of a resolved reply that could not be carried to its client's shard
/// (`xshard::hand_reply_home`): that shard gone, as its daemon stops, or this shard's carrier at its bound. Its resend
/// meets the completion record. (Until goal 238, 2026-11-18, a full channel there lost it too.)
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
    state.count(FULL, 1);
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
  state.count(PARKED, 1);
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

/// A verb resumed after waiting: what it needs to run under its own completion key and to answer where it waits.
pub(crate) struct ResumedVerb {
  /// Where to reply; `None` for a verb forwarded from another node, whose exchange polls the completion record.
  pub(crate) route: Option<ReplyRoute>,
  /// The completion key's origin.
  pub(crate) origin: u64,
  /// The request id.
  pub(crate) id: RequestId,
  /// The client it was asked by.
  pub(crate) client_id: u32,
  /// The principal it was asked as.
  pub(crate) principal: Principal,
  /// The verb.
  pub(crate) body: RequestBody,
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
  let verb = ResumedVerb {
    route,
    origin,
    id,
    client_id,
    principal,
    body,
  };
  let refusal = (!confirmed).then(|| {
    state.count(EXPIRED, 1);
    crate::verbs::refused(Refusal::LeaseUnconfirmed {
      version: state.fleet.configuration().version,
    })
  });
  run_resumed(state, verb, refusal, confirmed.then_some(SERVED));
}

/// Writes `reply`, unrecorded, where `route` says the request waits.
pub(crate) fn send_reply(state: &mut ShardState, route: ReplyRoute, reply: ReplyBody) {
  reply_home(state, route, reply);
}

/// Carries a resumed verb's reply to the client's shard as a forwarded verb's reply goes
/// (`crate::xshard::hand_reply_home`, §4.3, A-296): kept by this shard's carrier while that shard's channel is full, for
/// as long as it lives, and refused only for that shard gone or the carrier at its bound, counted. Handed over once with
/// no wait, a burst of resumed verbs (a green's works answered together, A-247) met full channels and lost their
/// replies, so their clients waited out their whole reply deadline: 5 a shard on every shard at 3,000 clients. A task
/// of this shard that waited for room fixed that (b49360733) until the carrier, but a full task arena refused the task,
/// and the reply with it.
fn reply_home(state: &mut ShardState, route: ReplyRoute, reply: ReplyBody) {
  if crate::xshard::hand_reply_home(route.shard, move || {
    crate::state::deliver(route.client_index, route.request, reply, true);
  })
  .is_err()
  {
    state.count(UNDELIVERED, 1);
  }
}

/// Runs a parked verb under its completion key, or answers its `refusal`: the reply recorded as its completion in one
/// durable step with its effects and delivered where the request waits; `served`, when given, counted once it is.
pub(crate) fn run_resumed(
  state: &mut ShardState,
  verb: ResumedVerb,
  refusal: Option<ReplyBody>,
  served: Option<&'static str>,
) {
  let ResumedVerb {
    route,
    origin,
    id,
    client_id,
    principal,
    body,
  } = verb;
  state.db.begin();
  state.current_request = Some((origin, id));
  state.reply_route = route;
  // Where the verb's volume stands in its journal before it runs, as `run_recorded` notes it (AC-3.3).
  let mark = body
    .volume()
    .and_then(|volume| crate::told::mark(state, crate::verbs::to_db_volume(volume)));
  let reply = match refusal {
    Some(refusal) => refusal,
    None => crate::verbs::dispatch(state, client_id, &principal, body),
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
    state.count(UNRECORDED, 1);
    return;
  }
  if let Some(served) = served {
    state.count(served, 1);
  }
  let Some(route) = route else {
    // A verb forwarded from another node: its exchange polls the completion record.
    return;
  };
  // A change the verb recorded is told to every FUSE mount of its volume before the reply (AC-3.3): held and sent by
  // a task of this shard; a reply that cannot be held goes now, counted.
  let (route, recorded) = match mark.and_then(|mark| crate::told::owed(state, mark)) {
    Some(owed) => match crate::told::hold_until_told(state, owed, route, recorded) {
      None => return,
      Some(unheld) => unheld,
    },
    None => (route, recorded),
  };
  reply_home(state, route, recorded);
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
      state.count(TIMER_REFUSED, 1);
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
    let _ = state::with_state_counted(resolve);
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

#[cfg(test)]
mod tests {
  use slates_db::catalog::Principal;
  use slates_db::register::HostId;
  use slates_ipc::protocol::{NamePolicy, Refusal, ReplyBody, RequestBody, SizeClass, VolumeId};
  use slates_wire::request::{RequestId, Seen};

  use super::{EXPIRED, SERVED, join, parked, resolve};
  use crate::state::ShardState;

  /// Format: the client every test request is asked by.
  const CLIENT: u32 = 7;
  /// Format: the sequence of the one request each test asks.
  const SEQUENCE: u32 = 1;

  /// What a test saw of the one read it asked, as the module's contract names it.
  #[derive(Debug, PartialEq, Eq)]
  struct ReadSeen {
    /// The read parked when it met the refusing lease: one waiter, nothing recorded.
    parked_at_first: bool,
    /// The reply recorded as the read's completion, by kind: `Some(true)` when it is the expected one.
    recorded_as_expected: Option<bool>,
    /// The module's count of verbs served after a wait ([`super::SERVED`]).
    served: u64,
    /// The module's count of verbs refused at their deadline ([`super::EXPIRED`]).
    expired: u64,
    /// The verbs parked once it resolved.
    parked_after: usize,
    /// Whether a retry of the request, once it resolved, still found it parked to join.
    joined_after: bool,
  }

  fn principal() -> Principal {
    Principal::Uid {
      host: HostId(0),
      uid: 1234,
    }
  }

  /// A volume named `name` made on `state`'s shard through the verbs, owned by [`principal`].
  fn volume_made(state: &mut ShardState, name: &str) -> Option<VolumeId> {
    let created = crate::verbs::dispatch(
      state,
      CLIENT,
      &principal(),
      RequestBody::Create {
        name: name.to_owned(),
        size: SizeClass::Dynamic { max: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    );
    match created {
      ReplyBody::Created { id } => Some(id),
      _ => None,
    }
  }

  /// Makes `state`'s owner lease refuse, as a newer version of the owner's own authority announced to it does: the one
  /// verdict an audit shard, whose lone owner's lease holds on no confirmation, can refuse with. The module asks only
  /// whether the verdict holds (`verbs::lease_verdict`), never why.
  fn lease_refuses(state: &mut ShardState) {
    state.lease = crate::lease::OwnerLease {
      superseded: Some(state.fleet.configuration().version.saturating_add(1)),
      ..crate::lease::OwnerLease::default()
    };
  }

  /// Lets `state`'s owner lease hold again, as a confirmation that reaches the shard does.
  fn lease_confirms(state: &mut ShardState) {
    state.lease = crate::lease::OwnerLease::default();
  }

  /// Asks `volume`'s status, a latest-state read, as request [`SEQUENCE`] of [`CLIENT`], run as `verbs::run_recorded`
  /// runs a request up to its commit: under its completion key, in one durable step, with no reply route (as a verb
  /// forwarded from another node, whose exchange reads the completion record). Whether it parked, with nothing
  /// recorded.
  fn ask_status(state: &mut ShardState, volume: VolumeId) -> bool {
    let id = RequestId {
      client: CLIENT,
      sequence: SEQUENCE,
    };
    state.db.begin();
    state.current_request = Some((state.origin_anchor.0, id));
    let reply = crate::verbs::dispatch(state, CLIENT, &principal(), RequestBody::Status { volume });
    state.current_request = None;
    let deferred = std::mem::take(&mut state.acceptance_deferred);
    let committed = state.db.commit(&mut state.segment).is_ok();
    deferred
      && committed
      && reply == super::parked_reply()
      && parked(state) == 1
      && recorded(state).is_none()
  }

  /// The reply recorded as the request's completion, `None` while none is.
  fn recorded(state: &ShardState) -> Option<ReplyBody> {
    match state
      .db
      .partition()
      .completion(state.origin_anchor.0, CLIENT, SEQUENCE)
    {
      Seen::Completed(bytes) => ReplyBody::from_bytes(&bytes).ok(),
      _ => None,
    }
  }

  fn counted(state: &ShardState, name: &str) -> u64 {
    state.refusals.get(name).copied().unwrap_or(0)
  }

  /// What `state` shows of the request once it resolved, `expected` judging its recorded reply.
  fn seen_after(
    state: &mut ShardState,
    parked_at_first: bool,
    expected: impl Fn(&ReplyBody) -> bool,
  ) -> ReadSeen {
    let id = RequestId {
      client: CLIENT,
      sequence: SEQUENCE,
    };
    let origin = state.origin_anchor.0;
    ReadSeen {
      parked_at_first,
      recorded_as_expected: recorded(state).map(|reply| expected(&reply)),
      served: counted(state, SERVED),
      expired: counted(state, EXPIRED),
      parked_after: parked(state),
      joined_after: join(state, origin, id),
    }
  }

  /// §4.8 "Leases and reads" (AUD-08), `lease_wait`'s first rule. Do: make a volume, let the owner lease refuse, and
  /// ask the volume's status as a recorded request; then let the lease confirm and resolve twice. Expect: the read
  /// parked with nothing recorded, then ran once its lease confirmed: its completion is the volume's status, counted
  /// served once and expired never; nothing is left parked, and a retry finds no request to join (it reads the
  /// completion). The second resolve changes nothing.
  #[test]
  fn a_parked_read_runs_once_its_lease_confirms_and_only_once() {
    let seen = crate::daemon::audit_on_shard(|state| {
      let volume = volume_made(state, "confirmed")?;
      lease_refuses(state);
      let parked_at_first = ask_status(state, volume);
      lease_confirms(state);
      resolve(state);
      resolve(state);
      Some(seen_after(state, parked_at_first, |reply| {
        matches!(reply, ReplyBody::Status { .. })
      }))
    });
    assert_eq!(
      seen,
      Some(ReadSeen {
        parked_at_first: true,
        recorded_as_expected: Some(true),
        served: 1,
        expired: 0,
        parked_after: 0,
        joined_after: false,
      })
    );
  }

  /// §4.8 "Leases and reads" (AUD-08), `lease_wait`'s second rule. Do: as above, but leave the lease refusing and put
  /// the waiter's deadline (the lease bound from its parking, which a unit test does not sleep through) behind the
  /// clock; resolve twice. Expect: the read refused `LeaseUnconfirmed` under the configuration's version and recorded
  /// so as its completion, counted expired once and served never; nothing is left parked, and a retry joins nothing.
  #[test]
  fn a_parked_read_whose_lease_never_confirms_is_refused_lease_unconfirmed_at_its_deadline() {
    let seen = crate::daemon::audit_on_shard(|state| {
      let volume = volume_made(state, "lapsed")?;
      lease_refuses(state);
      let parked_at_first = ask_status(state, volume);
      let version = state.fleet.configuration().version;
      for waiter in &mut state.lease_waiters.waiting {
        waiter.deadline_ns = 0;
      }
      resolve(state);
      resolve(state);
      Some(seen_after(state, parked_at_first, |reply| {
        matches!(
          reply,
          ReplyBody::Refused {
            refusal: Refusal::LeaseUnconfirmed { version: refused_under },
          } if *refused_under == version
        )
      }))
    });
    assert_eq!(
      seen,
      Some(ReadSeen {
        parked_at_first: true,
        recorded_as_expected: Some(true),
        served: 0,
        expired: 1,
        parked_after: 0,
        joined_after: false,
      })
    );
  }

  /// §4.8 "Leases and reads" (AUD-08), `lease_wait`'s wait: before its deadline a lease still refusing keeps the read
  /// parked, and a retry joins it rather than parking twice. Do: park the read, resolve with the lease still refusing,
  /// retry the request; then let the lease confirm and resolve. Expect: after the first resolve one waiter and nothing
  /// recorded; the retry joined, and still one waiter; then one completion, the volume's status, counted served once.
  #[test]
  fn a_read_still_within_its_deadline_stays_parked_and_its_retry_joins_it() {
    let seen = crate::daemon::audit_on_shard(|state| {
      let volume = volume_made(state, "waiting")?;
      lease_refuses(state);
      let asked = ask_status(state, volume);
      resolve(state);
      let id = RequestId {
        client: CLIENT,
        sequence: SEQUENCE,
      };
      let origin = state.origin_anchor.0;
      let joined = join(state, origin, id);
      let kept = parked(state) == 1 && recorded(state).is_none();
      lease_confirms(state);
      resolve(state);
      Some(seen_after(state, asked && joined && kept, |reply| {
        matches!(reply, ReplyBody::Status { .. })
      }))
    });
    assert_eq!(
      seen,
      Some(ReadSeen {
        parked_at_first: true,
        recorded_as_expected: Some(true),
        served: 1,
        expired: 0,
        parked_after: 0,
        joined_after: false,
      })
    );
  }
}
