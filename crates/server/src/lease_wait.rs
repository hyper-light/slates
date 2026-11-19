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
  use std::time::Duration;

  use slates_db::catalog::Principal;
  use slates_db::register::HostId;
  use slates_ipc::protocol::{NamePolicy, Refusal, ReplyBody, RequestBody, SizeClass, VolumeId};
  use slates_wire::request::RequestId;

  use super::{EXPIRED, SERVED, parked, resolve};
  use crate::state::ShardState;

  /// Format: the anchor of the node every test request is forwarded from: another node's.
  const ORIGIN: u64 = 1;
  /// Format: the client every test request is asked by.
  const CLIENT: u32 = 7;
  /// Format: the sequence of the one request each test asks, and retries under the same id.
  const SEQUENCE: u32 = 1;
  /// Shape: the pause between the deadline test's looks: a tenth of the fleet's heartbeat, short beside the lease bound
  /// (nine heartbeats), so a read resolved before its deadline by more than a pause is seen early.
  const LOOK_PAUSE_NS: u64 = crate::daemon::HEARTBEAT_NS / 10;
  /// Shape: how long past its deadline the deadline test looks for the refusal: half a liveness window, widened by the
  /// shard's measured descheduling, as `deadlines`' tests look for a release (A-660).
  const REFUSED_WITHIN_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS / 2;

  /// What a request through the owner's side of a forwarded verb was answered, by kind.
  #[derive(Debug, PartialEq, Eq)]
  enum Answered {
    /// Nothing yet: the request parked, or a retry joined the parked request.
    Waiting,
    /// The volume's status.
    Status,
    /// Refused `LeaseUnconfirmed` under the shard's configuration version, as the module refuses at a deadline.
    LeaseUnconfirmed,
    /// Anything else, as its debug text, so a failure shows it.
    Other(String),
  }

  /// When the deadline test saw its parked read resolved, against the read's deadline: the lease bound from its parking.
  #[derive(Debug, PartialEq, Eq)]
  enum Resolved {
    /// At its deadline or after it, within the bound the test looks for.
    AtItsDeadline,
    /// Seen resolved this many nanoseconds before its deadline.
    Early { before_deadline_ns: u64 },
    /// Still parked when the test stopped looking: how far past its deadline, and the shard's measured descheduling.
    NotWithin {
      past_deadline_ns: u64,
      overrun_ns: u64,
    },
  }

  /// What the confirmation test saw.
  #[derive(Debug, PartialEq, Eq)]
  struct Confirmed {
    /// The ask's answer.
    asked: Answered,
    /// The verbs parked while the lease refused.
    parked_while_refused: usize,
    /// The verbs parked once the lease held and the shard resolved twice.
    parked_once_resolved: usize,
    /// A retry under the same id once the read resolved, the lease refusing again.
    retried: Answered,
    /// The verbs parked after that retry.
    parked_after_retry: usize,
    /// The shard's `lease.wait.served` count.
    served: u64,
    /// The shard's `lease.wait.expired` count.
    expired: u64,
  }

  /// What the wait-and-join test saw.
  #[derive(Debug, PartialEq, Eq)]
  struct Joined {
    /// The ask's answer.
    asked: Answered,
    /// A retry under the same id after a resolve within the read's deadline, the lease still refusing.
    retried_while_parked: Answered,
    /// The verbs parked after that retry.
    parked_with_the_retry: usize,
    /// A second retry once the lease held and the shard resolved.
    retried_once_confirmed: Answered,
    /// The verbs parked after it.
    parked_after: usize,
    /// The shard's `lease.wait.served` count.
    served: u64,
    /// The shard's `lease.wait.expired` count.
    expired: u64,
  }

  /// What the deadline test saw.
  #[derive(Debug, PartialEq, Eq)]
  struct Lapsed {
    /// The ask's answer.
    asked: Answered,
    /// The verbs parked right after it.
    parked_at_first: usize,
    /// When the read was seen resolved.
    resolved: Resolved,
    /// The periods the daemon's fleet coordinator ran: none on a laptop daemon, so no fan of lease evidence could
    /// resolve the read and the module's own deadline timer did (the non-vacuity count).
    coordinator_periods: u64,
    /// A retry under the same id once the lease held again.
    retried: Answered,
    /// The verbs parked after it.
    parked_after: usize,
    /// The shard's `lease.wait.served` count.
    served: u64,
    /// The shard's `lease.wait.expired` count.
    expired: u64,
  }

  /// The principal every test volume is made and read as.
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

  /// Supersedes `state`'s owner lease as a holder's answer under the next configuration version does
  /// (`OwnerLease::supersede`): the shard's latest-state reads refuse until it installs that version. Returns it.
  fn superseded(state: &mut ShardState) -> u64 {
    let next = state.fleet.configuration().version.saturating_add(1);
    state.lease.supersede(next);
    next
  }

  /// Lets `state`'s owner lease hold again as installing the `superseding` version does (`OwnerLease::installed`).
  fn installed(state: &mut ShardState, superseding: u64) {
    state
      .lease
      .installed(superseding, slates_machine::clock::monotonic_ns());
  }

  /// Asks `volume`'s status, a latest-state read, as request [`SEQUENCE`] of [`CLIENT`] forwarded from [`ORIGIN`],
  /// through the owner's side of a forwarded verb (`verbs::run_forwarded`): the completion record first, a parked
  /// request's retry joined to it, else the verb run under its completion key.
  fn status_asked(state: &mut ShardState, volume: VolumeId) -> Answered {
    let id = RequestId {
      client: CLIENT,
      sequence: SEQUENCE,
    };
    let reply = crate::verbs::run_forwarded(
      state,
      ORIGIN,
      id,
      CLIENT,
      &principal(),
      RequestBody::Status { volume },
      None,
    );
    answered(state, reply)
  }

  /// `reply` by kind, a `LeaseUnconfirmed` judged against `state`'s configuration version.
  fn answered(state: &ShardState, reply: Option<ReplyBody>) -> Answered {
    let version = state.fleet.configuration().version;
    match reply {
      None => Answered::Waiting,
      Some(ReplyBody::Status { .. }) => Answered::Status,
      Some(ReplyBody::Refused {
        refusal: Refusal::LeaseUnconfirmed {
          version: refused_under,
        },
      }) if refused_under == version => Answered::LeaseUnconfirmed,
      Some(other) => Answered::Other(format!("{other:?}")),
    }
  }

  /// The count `name` on `state`'s shard.
  fn counted(state: &ShardState, name: &str) -> u64 {
    state.refusals.get(name).copied().unwrap_or(0)
  }

  /// §4.8 "Leases and reads" (AUD-08): a parked read runs the moment its lease confirms, and only once. Do: on an audit
  /// shard make a volume, supersede the owner lease, and ask the volume's status as a forwarded request; install the
  /// superseding version and resolve the shard's parked reads twice, as a fan of lease evidence installs it and resolves
  /// them (`fleet::fan_configs_to_shards`); supersede the lease again and retry the request under its id. Expect: the
  /// ask parked, unanswered, one verb parked; once resolved nothing is parked, counted served once and expired never;
  /// and the retry answered the status recorded as its completion though the lease refuses again, parking nothing: the
  /// read neither ran nor parked a second time.
  #[test]
  fn a_parked_read_runs_once_its_lease_confirms_and_only_once() {
    let seen = crate::daemon::audit_on_shard(|state| {
      let volume = volume_made(state, "confirmed")?;
      let superseding = superseded(state);
      let asked = status_asked(state, volume);
      let parked_while_refused = parked(state);
      installed(state, superseding);
      resolve(state);
      resolve(state);
      let parked_once_resolved = parked(state);
      superseded(state);
      let retried = status_asked(state, volume);
      Some(Confirmed {
        asked,
        parked_while_refused,
        parked_once_resolved,
        retried,
        parked_after_retry: parked(state),
        served: counted(state, SERVED),
        expired: counted(state, EXPIRED),
      })
    });
    assert_eq!(
      seen,
      Some(Confirmed {
        asked: Answered::Waiting,
        parked_while_refused: 1,
        parked_once_resolved: 0,
        retried: Answered::Status,
        parked_after_retry: 0,
        served: 1,
        expired: 0,
      })
    );
  }

  /// §4.8 "Leases and reads" (AUD-08): a parked read whose lease never confirms is refused `LeaseUnconfirmed` at its
  /// deadline, once. Do: on a laptop audit daemon make a volume, supersede the owner lease and ask the volume's status as
  /// a forwarded request; leave the lease refusing and look at the shard until the read resolves; then install the
  /// superseding version and retry the request under its id. Expect: the ask parked; the read stayed parked at every
  /// look before the lease bound from its parking had passed and was resolved after it, within half a liveness window
  /// past it widened by the shard's measured descheduling; the fleet coordinator ran no period (a laptop spawns none, so
  /// no fan of lease evidence resolved the read: the module's own deadline timer did); the retry answered the
  /// `LeaseUnconfirmed` recorded as its completion under the configuration's version though the lease now holds;
  /// nothing parked, counted expired once and served never.
  #[test]
  fn a_parked_read_whose_lease_never_confirms_is_refused_lease_unconfirmed_at_its_deadline() {
    let daemon = crate::daemon::audit_daemon();
    let seen = lapsed(&daemon);
    daemon.stop();
    assert_eq!(
      seen,
      Some(Lapsed {
        asked: Answered::Waiting,
        parked_at_first: 1,
        resolved: Resolved::AtItsDeadline,
        coordinator_periods: 0,
        retried: Answered::LeaseUnconfirmed,
        parked_after: 0,
        served: 0,
        expired: 1,
      })
    );
  }

  /// Parks a status read on `daemon`'s shard with its owner lease superseded, looks until the read resolves, then
  /// retries it with the lease holding again: what the deadline test saw, or `None` when the volume could not be made.
  fn lapsed(daemon: &crate::daemon::Daemon) -> Option<Lapsed> {
    let (volume, superseding, parked_at_ns, asked, parked_at_first) =
      crate::daemon::observe_first(daemon, |state| {
        let volume = volume_made(state, "lapsed")?;
        let superseding = superseded(state);
        let parked_at_ns = slates_machine::clock::monotonic_ns();
        let asked = status_asked(state, volume);
        Some((volume, superseding, parked_at_ns, asked, parked(state)))
      })?;
    let resolved = resolution(daemon, parked_at_ns);
    let coordinator_periods = daemon.fleet_progress();
    let (retried, parked_after, served, expired) =
      crate::daemon::observe_first(daemon, move |state| {
        installed(state, superseding);
        let retried = status_asked(state, volume);
        (
          retried,
          parked(state),
          counted(state, SERVED),
          counted(state, EXPIRED),
        )
      });
    Some(Lapsed {
      asked,
      parked_at_first,
      resolved,
      coordinator_periods,
      retried,
      parked_after,
      served,
      expired,
    })
  }

  /// Looks at `daemon`'s shard every [`LOOK_PAUSE_NS`] until its parked read is resolved: whether that was at or after
  /// its deadline (the lease bound from `parked_at_ns`), before it, or not within [`REFUSED_WITHIN_NS`] past it widened
  /// by the shard's measured descheduling (`slates_rt::futures::scheduler_overrun_ns`): a host that keeps the shard off
  /// its CPUs delays the deadline timer's wake by as much, which says nothing of the deadline it armed.
  fn resolution(daemon: &crate::daemon::Daemon, parked_at_ns: u64) -> Resolved {
    let deadline_ns = parked_at_ns.saturating_add(crate::lease::lease_bound_ns());
    loop {
      let (resolved, now_ns, overrun_ns) = crate::daemon::observe_first(daemon, |state| {
        (
          parked(state) == 0,
          slates_machine::clock::monotonic_ns(),
          slates_rt::futures::scheduler_overrun_ns(),
        )
      });
      if resolved {
        return resolved_at(now_ns, deadline_ns);
      }
      let past_deadline_ns = now_ns.saturating_sub(deadline_ns);
      if past_deadline_ns > REFUSED_WITHIN_NS.saturating_add(overrun_ns) {
        return Resolved::NotWithin {
          past_deadline_ns,
          overrun_ns,
        };
      }
      // The harness paces its looks; the daemon under test waits on its wheel.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_nanos(LOOK_PAUSE_NS));
    }
  }

  /// A read first seen resolved at `now_ns`, judged against its `deadline_ns`.
  fn resolved_at(now_ns: u64, deadline_ns: u64) -> Resolved {
    if now_ns < deadline_ns {
      Resolved::Early {
        before_deadline_ns: deadline_ns.saturating_sub(now_ns),
      }
    } else {
      Resolved::AtItsDeadline
    }
  }

  /// §4.8 "Leases and reads" (AUD-08): a read stays parked within its deadline, and a retry joins it. Do: on an audit
  /// shard make a volume, supersede the owner lease and ask the volume's status as a forwarded request; resolve the
  /// shard's parked reads while the lease still refuses and the read's deadline (the lease bound from its parking) is
  /// far off; retry the request under its id; then install the superseding version, resolve, and retry again. Expect:
  /// the ask and its first retry unanswered with one verb parked, so the retry joined the parked read rather than
  /// parking twice, and nothing was recorded; the second retry answered the status recorded as its completion, nothing
  /// parked, counted served once and expired never.
  #[test]
  fn a_read_still_within_its_deadline_stays_parked_and_its_retry_joins_it() {
    let seen = crate::daemon::audit_on_shard(|state| {
      let volume = volume_made(state, "waiting")?;
      let superseding = superseded(state);
      let asked = status_asked(state, volume);
      resolve(state);
      let retried_while_parked = status_asked(state, volume);
      let parked_with_the_retry = parked(state);
      installed(state, superseding);
      resolve(state);
      let retried_once_confirmed = status_asked(state, volume);
      Some(Joined {
        asked,
        retried_while_parked,
        parked_with_the_retry,
        retried_once_confirmed,
        parked_after: parked(state),
        served: counted(state, SERVED),
        expired: counted(state, EXPIRED),
      })
    });
    assert_eq!(
      seen,
      Some(Joined {
        asked: Answered::Waiting,
        retried_while_parked: Answered::Waiting,
        parked_with_the_retry: 1,
        retried_once_confirmed: Answered::Status,
        parked_after: 0,
        served: 1,
        expired: 0,
      })
    );
  }
}
