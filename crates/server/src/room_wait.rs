//! Admissions wait for the room the work in flight owes them: an extent's remake (A-789, Windows) and a destroyed
//! volume's teardown (A-884, every platform; main 01a20182's rulings of 2026-11-23).
//! A released extent's section is closed, so that its commit returns, then made again and held by the anchor
//! (`crate::content_extents::remake`): two anchor exchanges, while the extent is free but no claim can take it, and the
//! pool counts it pending rather than free (`crate::content_pool`). A create whose room only such extents would make is
//! **parked**, not refused: it runs again as each remake ends, and is refused with the room there is only once the
//! bound of the remakes in flight when it parked has passed and it still does not fit. A create that would not fit even
//! once every remake in flight ends is refused at once ([`verdict`]).
//!
//! **Why wait.** A destroy publishes, and a committed publication gives its volume's wholly free extents back to the
//! pool (A-98), so their remakes begin before the destroy answers. A create sent the moment it answered met them
//! unready and was refused `BudgetExceeded`, though its room was a remake away. The daemon holds the wait, not each
//! client: it learns the moment a remake ends, and its client sees one answer.
//!
//! **The bound.** One remake takes at most its two exchanges, each given up at the hold channel's learned answer bound
//! (`crate::content_extents::remake_bound_ns`, A-552), and the remakes in flight take turns on that one channel; so a
//! parked create waits at most that bound once for each remake in flight when it parked, the deadline it keeps however
//! often it runs again.
//!
//! **Teardowns (A-884).** A destroy is answered once its record says so; its teardown then frees the volume's objects
//! a slice a round (`verbs::step_destroys`), and only its end gives the volume's credits back: its reservation, its
//! inode allowance and its records (`verbs::release_slot_credits`). A create sent at once met them held and was refused
//! `BudgetExceeded`, though its room was a teardown away. Such a create parks as for a remake, whichever admission
//! refused it ([`Room`]), and runs again when a teardown ends ([`teardown_ended`]). Its bound is each teardown's
//! remaining slices at a step's budget apiece ([`teardown_window_ns`]), added to the remakes' bound, since a
//! publication after the teardown can begin remakes (Windows).
//!
//! **What holds.** As `crate::lease_wait` holds a parked verb: nothing of it runs and nothing is recorded until it is
//! resolved; it carries its completion key, principal and body, and runs through `verbs::dispatch` under its key
//! (`crate::lease_wait::run_resumed`). A retry of a parked request joins it ([`join`]). The list is bounded by the
//! shard's client credit (`crate::lease_wait::bound`); a create past it is refused at once, counted. A remake that ends
//! on one shard tells every other shard while any admission waits anywhere (`remade`, Windows').

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use slates_db::catalog::{Principal, VolumeState};
use slates_ipc::protocol::{RequestBody, SizeClass};
use slates_wire::request::RequestId;

use crate::merge_service::ReplyRoute;
use crate::state::{self, ShardState, VolumeSlot};

/// The status refusal count of a create parked for the room the work in flight owes it (the non-vacuity count of the
/// wait).
const PARKED: &str = "room.wait.parked";
/// The status refusal count of a parked create run again because a remake or a teardown ended or its deadline passed.
const RESUMED: &str = "room.wait.resumed";
/// The status refusal count of a create refused at its deadline, the work it waited for over and its room short.
const EXPIRED: &str = "room.wait.expired";
/// The status refusal count of a create refused at once because the waiter list was full.
const FULL: &str = "room.wait.full";
/// The status refusal count of a deadline timer the shard's arena refused (waiters are still run at the next remake's
/// end).
const TIMER_REFUSED: &str = "room.wait.timer_refused";
/// The status refusal count of a remake's end another shard could not be told of (that shard gone, or this shard's
/// carrier at its bound): its waiters run at their deadline.
#[cfg(windows)]
const UNTOLD: &str = "room.wait.untold";

/// The creates parked on every shard of this process: a remake that ends tells the other shards only while one waits.
static PARKED_ANYWHERE: AtomicUsize = AtomicUsize::new(0);

/// One parked create.
struct RoomWaiter {
  route: Option<ReplyRoute>,
  origin: u64,
  id: RequestId,
  client_id: u32,
  principal: Principal,
  body: RequestBody,
  deadline_ns: u64,
}

/// A shard's parked creates.
#[derive(Default)]
pub(crate) struct RoomWaiters {
  waiting: Vec<RoomWaiter>,
  /// The completion keys parked, so a retry joins rather than parks twice.
  by_request: BTreeSet<(u64, u32, u32)>,
  /// Whether this shard's deadline timer task is running.
  timer_armed: bool,
  /// Set by an admission refused for room the work in flight would make ([`note_refusal`]), taken by the verb's
  /// dispatch, which parks it ([`park_if_owed`]).
  owed: bool,
  /// The deadline of the waiter being run again ([`resolve`]): a create refused again for room still owed parks under
  /// it, never under a later one.
  resuming: Option<u64>,
}

/// What an admission refused for room does (A-789).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoomVerdict {
  /// The remakes in flight would make the room: wait for them.
  Wait,
  /// No remake in flight, or not room enough even once each ends: refused now, with the room there is.
  Refuse,
}

/// The verdict on an admission of `requested` bytes refused with `admittable` bytes of room, while `pending` bytes of
/// the pool's extents are being made again: wait while those would make the room, else refuse at once.
pub(crate) fn verdict(requested: u64, admittable: u64, pending: u64) -> RoomVerdict {
  if pending > 0 && requested <= admittable.saturating_add(pending) {
    RoomVerdict::Wait
  } else {
    RoomVerdict::Refuse
  }
}

/// Which of a create's admissions refused it for room. Each is room a destroy in flight gives back when its teardown
/// ends (`verbs::release_slot_credits`), and the byte budget is room the remakes of the pool's extents make as well
/// (Windows, A-789).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Room {
  /// The shard's byte budget: a bounded volume's reservation.
  Bytes,
  /// The shard's version slab: a bounded volume's inode allowance (§4.2 inode dimension).
  Versions,
  /// The shard's metadata ledger: a volume's records (§4.2 metadata dimension).
  Metadata,
}

/// Notes the running verb's bounded admission of `requested` of `room` refused with `available` of it: owed a wait when
/// the work in flight would make that room ([`verdict`]): the teardowns of the volumes this shard is destroying, and
/// for bytes the remakes of the pool's extents. Whether it is.
pub(crate) fn note_refusal(
  state: &mut ShardState,
  room: Room,
  requested: u64,
  available: u64,
) -> bool {
  let teardowns = teardown_room(state, room);
  let pending = match room {
    Room::Bytes => state.store.pending_room().saturating_add(teardowns),
    Room::Versions | Room::Metadata => teardowns,
  };
  let owed = verdict(requested, available, pending) == RoomVerdict::Wait;
  state.room_waiters.owed = owed;
  owed
}

/// This shard's volumes whose destroy is recorded and whose teardown has not ended: what `verbs::step_destroys` steps.
fn destroying(state: &ShardState) -> impl Iterator<Item = &VolumeSlot> {
  state
    .by_id
    .iter()
    .filter(|(id, _)| {
      state
        .db
        .partition()
        .volume(**id)
        .is_some_and(|record| record.state == VolumeState::Destroying)
    })
    .filter_map(|(_, handle)| state.volumes.get(*handle).ok())
}

/// The room of kind `room` the teardowns in flight on this shard give back when they end: each destroying volume's
/// credits of that kind, as `verbs::release_slot_credits` releases them.
fn teardown_room(state: &ShardState, room: Room) -> u64 {
  destroying(state).fold(0u64, |sum, slot| {
    let credits = match room {
      Room::Bytes => slot
        .reservation
        .map_or(0, |reservation| reservation.bytes)
        .saturating_add(slot.volume.budget_hold()),
      Room::Versions => slot
        .version_credit
        .map_or(0, |credit| credit.slots)
        .saturating_add(slot.volume.version_hold()),
      Room::Metadata => slot
        .metadata_credit
        .map_or(0, |credit| credit.bytes)
        .saturating_add(slot.volume.journal_grown()),
    };
    sum.saturating_add(credits)
  })
}

/// Derived: how long a create parked now may wait for the teardowns in flight on this shard, nanoseconds: each
/// destroying volume's remaining slices (`Volume::destroy_slices_left`, at its last slice's pace) at a step's budget
/// apiece (`config.step_quantum_ns`), since the shard runs one slice of each teardown a round of its serve loop
/// (`verbs::step_destroys`). Zero with none in flight.
fn teardown_window_ns(state: &ShardState) -> u64 {
  let step = state.config.step_quantum_ns().max(1);
  // A teardown not begun yet (a guest device still serves its volume) is at least the round that begins it.
  let slices = destroying(state).fold(0u64, |sum, slot| {
    let left = u64::try_from(slot.volume.destroy_slices_left()).unwrap_or(u64::MAX);
    sum.saturating_add(left.max(1))
  });
  slices.saturating_mul(step)
}

/// A teardown ended on this shard (`verbs::step_destroys`, once the destroyed volume's credits are given back): the
/// creates parked here run again.
pub(crate) fn teardown_ended(state: &mut ShardState) {
  resolve(state);
}

/// Whether `body` is a verb that may park for room: a create of a bounded volume, whose admission reserves its bytes
/// whole and refuses with nothing changed, so running it again later is running it once.
pub(crate) fn parkable(body: &RequestBody) -> bool {
  matches!(
    body,
    RequestBody::Create {
      size: SizeClass::Bounded { .. },
      ..
    }
  )
}

/// Clears what a verb before this one may have noted, so only this verb's own admission decides ([`park_if_owed`]).
pub(crate) fn begin(state: &mut ShardState) {
  state.room_waiters.owed = false;
}

fn key(origin: u64, id: RequestId) -> (u64, u32, u32) {
  (origin, id.client, id.sequence)
}

/// Parks the running verb (`body` from `client_id` as `principal`) when its admission was refused for room the work in
/// flight owes it ([`note_refusal`]), marking it deferred so `run_recorded` records nothing and writes no reply.
/// `false` when it is not owed a wait, no recorded request is running, the list is full, or its deadline has passed:
/// the caller answers the refusal it has.
pub(crate) fn park_if_owed(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
) -> bool {
  if !std::mem::take(&mut state.room_waiters.owed) {
    return false;
  }
  let Some((origin, id)) = state.current_request else {
    return false;
  };
  if state.room_waiters.waiting.len() >= crate::lease_wait::bound(state) {
    state.count(FULL, 1);
    return false;
  }
  let now = slates_machine::clock::monotonic_ns();
  let window_ns = remake_window_ns().saturating_add(teardown_window_ns(state));
  let deadline_ns = state
    .room_waiters
    .resuming
    .unwrap_or_else(|| now.saturating_add(window_ns));
  if now >= deadline_ns {
    state.count(EXPIRED, 1);
    return false;
  }
  let route = state.reply_route.take();
  state.room_waiters.waiting.push(RoomWaiter {
    route,
    origin,
    id,
    client_id,
    principal: principal.clone(),
    body,
    deadline_ns,
  });
  state.room_waiters.by_request.insert(key(origin, id));
  state.acceptance_deferred = true;
  state.count(PARKED, 1);
  PARKED_ANYWHERE.fetch_add(1, Ordering::AcqRel);
  arm_timer(state);
  true
}

/// A retry of a parked request joins it: it is answered when the parked create runs again and is answered. Returns
/// whether the request was parked.
pub(crate) fn join(state: &mut ShardState, origin: u64, id: RequestId) -> bool {
  if !state.room_waiters.by_request.contains(&key(origin, id)) {
    return false;
  }
  // The newest route wins: a retry on a reconnected client is answered where it now waits.
  if let Some(route) = state.reply_route.take()
    && let Some(waiter) = state
      .room_waiters
      .waiting
      .iter_mut()
      .find(|waiter| waiter.origin == origin && waiter.id == id)
  {
    waiter.route = Some(route);
  }
  true
}

/// Runs every parked create again: one that fits now is admitted, one still owed room the work in flight will make
/// parks again under its own deadline, and one past its deadline is refused with the room there is. Called when a
/// remake or a teardown ends and at each deadline.
pub(crate) fn resolve(state: &mut ShardState) {
  if state.room_waiters.waiting.is_empty() {
    return;
  }
  let waiting = std::mem::take(&mut state.room_waiters.waiting);
  for waiter in waiting {
    state
      .room_waiters
      .by_request
      .remove(&key(waiter.origin, waiter.id));
    let _ = PARKED_ANYWHERE.fetch_update(Ordering::AcqRel, Ordering::Acquire, |parked| {
      parked.checked_sub(1)
    });
    run(state, waiter);
  }
}

/// Runs one parked create again under its completion key, its deadline kept for a park it meets again.
fn run(state: &mut ShardState, waiter: RoomWaiter) {
  let RoomWaiter {
    route,
    origin,
    id,
    client_id,
    principal,
    body,
    deadline_ns,
  } = waiter;
  state.count(RESUMED, 1);
  state.room_waiters.resuming = Some(deadline_ns);
  crate::lease_wait::run_resumed(
    state,
    crate::lease_wait::ResumedVerb {
      route,
      origin,
      id,
      client_id,
      principal,
      body,
    },
    None,
    None,
  );
  state.room_waiters.resuming = None;
}

/// An extent's remake ended (A-789): this shard's parked creates run again, and every other shard's, told while any
/// create waits anywhere. Called by the remake's task on the shard that released the extent; only Windows remakes one.
#[cfg(windows)]
pub(crate) fn remade() {
  let Some((origin, shards)) = state::with_state_counted(|s| {
    resolve(s);
    (s.shard, s.shards.clone())
  }) else {
    return;
  };
  if PARKED_ANYWHERE.load(Ordering::Acquire) == 0 {
    return;
  }
  for shard in shards.into_iter().filter(|shard| *shard != origin) {
    crate::xshard::run_on_waiting(origin, shard, resolve, UNTOLD);
  }
}

/// Derived: how long a create parked now may wait, nanoseconds: one remake's bound
/// (`crate::content_extents::remake_bound_ns`) for each remake in flight, at least one, since they take turns on the
/// anchor's one hold channel.
#[cfg(windows)]
fn remake_window_ns() -> u64 {
  let remakes = u64::try_from(crate::content_extents::remakes_in_flight())
    .unwrap_or(u64::MAX)
    .max(1);
  crate::content_extents::remake_bound_ns().saturating_mul(remakes)
}

/// [`remake_window_ns`] where no extent is ever made again (Linux and macOS give a released extent back at once): no
/// wait for remakes; a teardown in flight is waited for on every platform ([`teardown_window_ns`]).
#[cfg(not(windows))]
fn remake_window_ns() -> u64 {
  0
}

/// Arms this shard's deadline timer if it is not running: one task that sleeps until the earliest deadline, runs the
/// parked creates again, and goes on while any is parked (one per shard, so bounded).
fn arm_timer(state: &mut ShardState) {
  if state.room_waiters.timer_armed {
    return;
  }
  match slates_rt::futures::spawn(deadline_timer()) {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
      state.room_waiters.timer_armed = true;
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
        .room_waiters
        .waiting
        .iter()
        .map(|waiter| waiter.deadline_ns)
        .min();
      match earliest {
        Some(deadline) => Some(deadline.saturating_sub(now)),
        None => {
          s.room_waiters.timer_armed = false;
          None
        }
      }
    })
    .flatten();
    let Some(wait) = wait else {
      return;
    };
    // A refused sleep (the timer wheel full) still runs the creates now; the next turn sleeps again.
    let _ = slates_rt::futures::sleep(wait.max(1)).await;
    let _ = state::with_state_counted(resolve);
  }
}

#[cfg(test)]
mod tests {
  use super::{RoomVerdict, verdict};

  /// A-789, main 01a20182's ruling of 2026-11-23. Do: judge admissions refused for room with remakes in flight and
  /// without, the room short by less than they would make, by exactly it, and by more. Expect: a wait only while remakes
  /// are in flight and would make the room; with none in flight, or not enough even once each ends, a refusal at once.
  #[test]
  fn an_admission_waits_only_for_room_the_remakes_in_flight_would_make() {
    let (room, pending) = (4u64 << 30, 2u64 << 30);
    assert_eq!(verdict(room + 1, room, pending), RoomVerdict::Wait);
    assert_eq!(verdict(room + pending, room, pending), RoomVerdict::Wait);
    assert_eq!(
      verdict(room + pending + 1, room, pending),
      RoomVerdict::Refuse
    );
    assert_eq!(verdict(room + 1, room, 0), RoomVerdict::Refuse);
    assert_eq!(verdict(u64::MAX, u64::MAX, u64::MAX), RoomVerdict::Wait);
    assert_eq!(verdict(1, 0, 0), RoomVerdict::Refuse);
  }
}
