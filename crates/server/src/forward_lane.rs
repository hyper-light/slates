//! The forwards to one owner, multiplexed over the one record session this node holds to it (§4.8 "Lookup", §4.10;
//! the network load test's fault one, GAPS).
//!
//! **What it fixes.** A verb a node cannot serve locally is forwarded to its owner over the record session to that
//! peer (`fleet::FORWARD_STREAM`). Each forward used to borrow the session for its whole round trip
//! ([`crate::fleet::take_sessions`]), and every other forward to that owner polled for it on a 10 ms sleep. So the
//! forwards of a client node's clients went one at a time. At 300 clients a forwarded `CreateWork` took 1.71 s at
//! the median, and at 1,000 it took 4.30 s (45 µs on the owner's own node). A forward that waited on a held owner
//! shard made every forward behind it wait too (`forwards_to_one_owner_never_wait_behind_each_other`).
//!
//! **The shape.** The session already runs several exchanges at once, each on its own stream (`Endpoint::begin`,
//! `take_reply`, `drive`). So each peer has one lane in the control shard's state: the forwards queued, the
//! exchanges open, the replies taken. Whichever waiting forwarder finds the lane undriven drives it, one turn at a
//! time. A turn borrows the session, abandons what forwarders gave up, begins every queued forward and takes every
//! reply that has arrived, waking its forwarder. It then drives the session until a packet or a timer, or until a
//! forward is newly queued (a new forward wakes the driver, so it never waits for the open ones' silence; one still
//! queued behind the session's stream credit does not end the turn), returns the session, and yields to the shard
//! before its next turn. Between turns the session is back in its slot, so record commits and discovery pages borrow it as
//! before. A forwarder whose replies have all come stops driving and wakes the next waiter. An exchange left open
//! is taken by whichever forwarder drives next.
//!
//! **Bounds.** A lane holds at most [`lane_bound`] forwards (queued and open together): the requests this node's
//! clients may hold in flight, `clients_per_shard × slots` on each shard. Past it a forward is refused at once
//! (`fleet.forward.lane_full`), never queued. The session's own stream credit bounds the open exchanges, and a
//! forward past it waits queued (`Backlogged`, refused typed by the endpoint) until a stream frees. Each forwarder
//! bounds its own wait: the batch is given up once none of its forwards has made progress for its stall budget,
//! where progress is a reply completed or bytes of one of its own replies read. Its open exchanges are abandoned on
//! the next turn and its queued ones dropped. Until 2026-11-07 progress was any byte the session consumed while a
//! forward was open: a session busy with other replies kept a forward its owner never answered alive for good (298 in
//! fixtures' 1,000-client primary-kill rung, the owner unable to commit and the session full of `LeaseUnconfirmed`
//! replies), so the origin's resend and its `OwnerUnreachable` never came.
//!
//! **A dead session.** A session whose peer is gone without a word is judged where a borrower hands it back
//! (`fleet::return_sessions`, the transport's `peer_silent_past`), and the link dials again; every borrower's hold is
//! bounded, so a borrowed session is judged within its window plus one hold. The judgement runs only when a borrower
//! returns, so how late past its window a silent session is dropped is the lane's return cadence: 0 to 209 ms past a
//! 1,998 ms window under a 256-client burst, in the record's runs. One sitting unborrowed in its slot is the link's
//! discovery refresh's to catch, and a peer's stateless reset ends one sooner where it is heard. That division is why
//! no timer judges it (the bug record of 2026-11-09, a dead record session was lent on). The lane's forwards on a
//! dropped session are answered as unanswered by its next settle.
//!
//! **What a session change does.** An exchange is tied to the session it was begun on (its connection id). If the
//! link replaced the session meanwhile, the forward is answered as unanswered, as a forward cut off by its session's
//! loss always was, and the caller's refusal and retry path takes it from there.
//!
//! **A resend's place (A-306).** The lane begins its queued forwards in the order of their first send ([`Place`]): the
//! caller's resend of a forward given up or cut off is queued where its first send was, ahead of every forward first
//! queued after it. A resend queued at the back, as each was until 2026-11-18, was given up again before it was begun
//! behind a backlog longer than a stall budget, while later forwards went ahead: a read through a client node whose
//! session to its owner died was refused at its failover bound in 1 of 10 runs behind a 256-client burst (2.3 to
//! 10.5 s), and answered in 4.1 to 6.5 s, 10 of 10, with its place kept (the bug record of 2026-11-18).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::task::{Poll, Waker};

use slates_cluster::BatchReplies;
use slates_db::HostId;
use slates_rt::futures;
use slates_transport::connection::Priority;
use slates_transport::endpoint::{ConnectionId, Endpoint, EndpointError};

use crate::state;

/// The status count of a failed or silent session dropped (a peer's reset, the transport's silence rule, a discovery page
/// that failed) whose replacement the lane had a reply on within a liveness budget of the drop: the property the
/// forward path owes after any such drop, whoever dropped it.
const REPLACED_ANSWERED_IN_BUDGET: &str = "fleet.forward.replaced_answered_in_budget";
/// Of the same drops, those whose replacement's first reply came later than a liveness budget after the drop.
const REPLACED_ANSWERED_PAST_BUDGET: &str = "fleet.forward.replaced_answered_past_budget";
/// Format: nanoseconds per millisecond, for the lane's log lines.
const NS_PER_MS: u64 = 1_000_000;
/// The status count of a forward refused because its lane held [`lane_bound`] forwards already.
const LANE_FULL: &str = "fleet.forward.lane_full";
/// The status count of a forward answered unanswered because its session was replaced while it was open.
const SESSION_REPLACED: &str = "fleet.forward.session_replaced";
/// The status count of a forward begun on the session (the non-vacuity count of the multiplexed path).
const BEGUN: &str = "fleet.forward.begun";
/// The status count of a waiting forwarder woken by the driver with its reply taken: a reply begun on the session
/// and taken by another forwarder's turn (the count that shows forwards sharing the session rather than taking turns).
const TAKEN_FOR_ANOTHER: &str = "fleet.forward.taken_for_another";
/// The status count of a forwarder whose forwards were all answered (or answered unanswered by a replaced session):
/// with [`BATCH_GIVEN_UP`], every forwarder that ended, so the forwarders still waiting are the ones begun and not
/// counted here.
const BATCH_DONE: &str = "fleet.forward.batch_done";
/// The status count of a forwarder that gave up its batch at its stall budget, or because its session failed.
const BATCH_GIVEN_UP: &str = "fleet.forward.batch_given_up";
/// The status count of a batch resent at the place its first issue had in the lane ([`Place`]): the non-vacuity count
/// of the kept place, so a test that holds a resend's order shows the resends it held were made.
const RESENT_IN_PLACE: &str = "fleet.forward.resent_in_place";

/// Where one forward is.
enum Stage {
  /// Waiting to be begun on the session.
  Queued,
  /// Begun as `exchange` on the session whose connection id is `session`.
  Open {
    exchange: u64,
    session: ConnectionId,
  },
  /// Answered (`Some`), or unanswered (`None`: its session was replaced, or the session refused it).
  Done(Option<Vec<u8>>),
}

/// One forward in a lane.
struct Ticket {
  /// The forwarder that queued it.
  forwarder: u64,
  request: Vec<u8>,
  stage: Stage,
  /// When it last made progress: queued, begun, or bytes of its own reply read while it was open.
  progress_ns: u64,
  /// Its place in the lane's queue ([`Place`]): its own ticket when first issued, the first issue's when resent.
  place: u64,
}

/// A forward's place in its lane (A-306): the ticket its first issue was given. A resend of the same requests under
/// their own ids (`verbs::resolve_and_forward`, after a forward went unanswered or was never begun within its stall
/// budget) is queued at that place, so it is begun ahead of every forward first queued after it, as if it had waited
/// there all along. Queued at the back with fresh tickets, as every resend was before, a forward lost its place at each
/// budget: behind a backlog longer than a budget it was given up before it was begun, again and again, while forwards
/// first queued after it went ahead, until its failover bound refused it (a read through a client node whose session to
/// the owner went dead met `OwnerUnreachable` after 10 s behind the resends of a 256-client burst, about 45,000 to
/// 57,000 batches given up; the bug record of 2026-11-09). Kept by the caller across its resends; `Place::default()`
/// for a first issue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Place {
  /// The first ticket of the first issue; `None` until the first issue is queued.
  first: Option<u64>,
}

/// One peer's forwards.
#[derive(Default)]
struct Lane {
  /// Forwards waiting to be begun, by their place and then their ticket ([`Place`]): a first issue's place is its own
  /// ticket, and ticket ids rise as they are issued, so forwards are begun in the order they were first queued and a
  /// resend keeps the place its first issue had. A set, so a forwarder leaving takes its own out at log cost: a scan of
  /// the queue on each leave was quadratic in a burst.
  queued: BTreeSet<(u64, u64)>,
  tickets: BTreeMap<u64, Ticket>,
  /// The open forwards by their exchange on the session they were begun on ([`Lane::open_on`]): a turn takes only
  /// the replies that completed and marks only the exchanges that read bytes, as the session reports them
  /// (`Endpoint::take_completed`, `take_progressed`), never asking every open forward. Asking every one, a drain of
  /// the session each, held the client node's control shard 1.09 s in one poll with about 20,000 forwards open
  /// (the f = 1 primary-kill rung, 2026-11-08, 6 of 6 runs; the watchdog withheld its beat and the anchor killed it).
  open: BTreeMap<u64, u64>,
  /// The session the open forwards were begun on.
  open_on: Option<ConnectionId>,
  /// Whether a forward was queued since the driver's last settle: what ends a driving turn early so the forward is
  /// begun at once. Forwards still queued from before, waiting for the session's stream credit, do not: a turn that
  /// returned while any forward was queued returned at once whenever the credit was full, and its forwarder turned
  /// again in the same poll without end. With about 20,000 forwards waiting on a restarted owner, that held the client
  /// node's control shard 1.08 s in one poll (`forward_to_owner` → `forward_batch_over_leader_session` →
  /// `drive_or_queued`, sampled with frame pointers in the f = 1 primary-kill rung, 2026-11-08), and the watchdog
  /// withheld its beat.
  fresh_queued: bool,
  /// Exchanges whose forwarders gave up while they were open, to abandon on the next turn, with their session.
  abandon: Vec<(u64, ConnectionId)>,
  /// Whether a forwarder is driving the lane now.
  driving: bool,
  /// The driver's waker while it waits inside a turn, woken when a forward is queued.
  driver: Option<Waker>,
  /// The waiting forwarders' wakers.
  waiters: BTreeMap<u64, Waker>,
}

/// Every peer's lane, in the control shard's state.
#[derive(Default)]
pub(crate) struct ForwardLanes {
  lanes: BTreeMap<HostId, Lane>,
  next_ticket: u64,
  next_forwarder: u64,
  /// Each peer serve loop's finished forwards ([`ServedForwards`]), by the loop's slot.
  served: BTreeMap<u64, ServeSlot>,
  next_serve: u64,
}

/// Derived: the forwards one lane may hold, queued and open together: every request this node's clients may hold in
/// flight (`clients_per_shard × slots`, each shard's credit, over every shard). Forwards come only from those
/// requests (and the cold control verbs), so a lane at its bound means a fault, refused rather than queued.
fn lane_bound(state: &state::ShardState) -> usize {
  crate::config::forward_lane_bound(
    state.config.clients_per_shard,
    state.config.region.slots,
    u16::try_from(state.shards.len()).unwrap_or(u16::MAX),
  )
  .get()
}

/// Queues `requests` on `peer`'s lane under a new forwarder at `place` ([`ForwardLanes::queue`]): its id and its
/// tickets, in order, or `None` when the lane is at its bound (counted) or the state is out of reach. A resend at a
/// kept place is counted ([`RESENT_IN_PLACE`]).
fn enqueue(peer: HostId, requests: &[Vec<u8>], place: &mut Place) -> Option<(u64, Vec<u64>)> {
  state::with_state(|s| {
    let bound = lane_bound(s);
    let resend = place.first.is_some();
    let queued = s
      .forward_lanes
      .queue(peer, requests, place, bound, futures::now_ns());
    if queued.is_none() {
      s.count(LANE_FULL, 1);
    } else if resend {
      s.count(RESENT_IN_PLACE, 1);
    }
    queued
  })
  .flatten()
}

impl ForwardLanes {
  /// Queues `requests` on `peer`'s lane under a new forwarder at `place`, at `now`: its id and its tickets, in order,
  /// or `None` when the lane would hold more than `bound`. A first issue takes its own tickets as its place and records
  /// it in `place`; a resend (`place` already recorded) is queued at the first issue's place, each request at the
  /// ticket its first issue had (they were issued together, so in a run). The waiting driver is woken.
  fn queue(
    &mut self,
    peer: HostId,
    requests: &[Vec<u8>],
    place: &mut Place,
    bound: usize,
    now: u64,
  ) -> Option<(u64, Vec<u64>)> {
    let held = self.lanes.get(&peer).map_or(0, |lane| lane.tickets.len());
    if held.saturating_add(requests.len()) > bound {
      return None;
    }
    self.next_forwarder = self.next_forwarder.saturating_add(1);
    let forwarder = self.next_forwarder;
    let mut tickets = Vec::with_capacity(requests.len());
    for _ in requests {
      self.next_ticket = self.next_ticket.saturating_add(1);
      tickets.push(self.next_ticket);
    }
    let first = place.first;
    let lane = self.lanes.entry(peer).or_default();
    for (offset, (ticket, request)) in tickets.iter().zip(requests).enumerate() {
      let at = first.map_or(*ticket, |first| {
        first.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX))
      });
      lane.tickets.insert(
        *ticket,
        Ticket {
          forwarder,
          request: request.clone(),
          stage: Stage::Queued,
          progress_ns: now,
          place: at,
        },
      );
      lane.queued.insert((at, *ticket));
    }
    lane.fresh_queued = true;
    if let Some(driver) = lane.driver.take() {
      driver.wake();
    }
    if first.is_none() {
      place.first = tickets.first().copied();
    }
    Some((forwarder, tickets))
  }

  /// Removes `forwarder`'s tickets from `peer`'s lane: an open one's exchange is abandoned on the next turn, and if it
  /// was driving, the lane is left undriven and the next waiter woken to drive it. A lane left with nothing is dropped.
  fn leave(&mut self, peer: HostId, forwarder: u64, tickets: &[u64], was_driving: bool) {
    let Some(lane) = self.lanes.get_mut(&peer) else {
      return;
    };
    for ticket in tickets {
      let Some(entry) = lane.tickets.remove(ticket) else {
        continue;
      };
      lane.queued.remove(&(entry.place, *ticket));
      if let Stage::Open { exchange, session } = entry.stage {
        lane.open.remove(&exchange);
        lane.abandon.push((exchange, session));
      }
    }
    lane.waiters.remove(&forwarder);
    if was_driving {
      lane.driving = false;
      lane.driver = None;
      if let Some(next) = lane.waiters.values().next() {
        next.wake_by_ref();
      }
    }
    if lane.tickets.is_empty() && lane.abandon.is_empty() && !lane.driving {
      self.lanes.remove(&peer);
    }
  }
}

/// What a forwarder sees of its tickets now.
enum Look {
  /// Every ticket is done: the replies, in order.
  Done(Vec<Option<Vec<u8>>>),
  /// Not all done; whether this forwarder should drive (nobody does), and its latest progress.
  Waiting { drive: bool, progress_ns: u64 },
}

/// Looks at `tickets`; when this forwarder should drive, marks the lane driven by it.
fn look(peer: HostId, tickets: &[u64]) -> Option<Look> {
  state::with_state(|s| {
    let lane = s.forward_lanes.lanes.get_mut(&peer)?;
    let mut progress_ns = 0;
    let mut done = Vec::with_capacity(tickets.len());
    for ticket in tickets {
      match lane
        .tickets
        .get(ticket)
        .map(|ticket| (&ticket.stage, ticket.progress_ns))
      {
        Some((Stage::Done(reply), _)) => done.push(reply.clone()),
        Some((_, at)) => progress_ns = progress_ns.max(at),
        None => done.push(None),
      }
    }
    if done.len() == tickets.len() {
      return Some(Look::Done(done));
    }
    let drive = !lane.driving;
    if drive {
      lane.driving = true;
    }
    Some(Look::Waiting { drive, progress_ns })
  })
  .flatten()
}

/// Removes `forwarder`'s tickets from `peer`'s lane ([`ForwardLanes::leave`]).
fn leave(peer: HostId, forwarder: u64, tickets: &[u64], was_driving: bool) {
  let _ = state::with_state(|s| s.forward_lanes.leave(peer, forwarder, tickets, was_driving));
}

/// A forwarder's hold on its tickets: dropped however the forwarder ends (its replies taken, its budget spent, its
/// future dropped), it leaves the lane, so no ticket outlives its forwarder and a driver's end always hands the lane on.
struct Held {
  peer: HostId,
  forwarder: u64,
  tickets: Vec<u64>,
  driving: bool,
}

impl Drop for Held {
  fn drop(&mut self) {
    leave(self.peer, self.forwarder, &self.tickets, self.driving);
  }
}

/// Forwards `requests` to `peer` over its record session, sharing the session with every other forward to it: the
/// replies in order (`None` for one not answered), or `None` when the lane was full or the session never came within
/// `stall_ns` (nothing was begun). `streamed_bytes` and `streamed_ns` are this batch's own: its replies' bytes after
/// the first, and the time from the first reply to the last (the session's bytes are no longer one batch's). The batch
/// is queued at `place` ([`Place`]): a caller resending the same requests passes the place it kept from their first
/// issue, which this records. Called from the control shard, where the record sessions live.
pub(crate) async fn forward_batch(
  peer: HostId,
  requests: &[Vec<u8>],
  stall_ns: u64,
  place: &mut Place,
) -> Option<BatchReplies> {
  let (forwarder, tickets) = enqueue(peer, requests, place)?;
  let mut held = Held {
    peer,
    forwarder,
    tickets,
    driving: false,
  };
  let began = futures::now_ns();
  let mut begun = false;
  let mut first_reply: Option<(u64, usize)> = None;
  let mut missed = false;
  loop {
    let look = look(peer, &held.tickets)?;
    let (drive, progress_ns) = match look {
      Look::Done(replies) => {
        state::with_state(|s| s.count(BATCH_DONE, 1));
        let last_ns = futures::now_ns();
        let bytes: usize = replies.iter().flatten().map(Vec::len).sum();
        let (first_ns, first_len) = first_reply.unwrap_or((last_ns, bytes));
        return Some(BatchReplies {
          replies,
          streamed_bytes: u64::try_from(bytes.saturating_sub(first_len)).unwrap_or(u64::MAX),
          streamed_ns: last_ns.saturating_sub(first_ns),
        });
      }
      Look::Waiting { drive, progress_ns } => (drive, progress_ns),
    };
    held.driving = held.driving || drive;
    let quiet_ns = futures::now_ns().saturating_sub(progress_ns.max(began));
    if quiet_ns >= stall_ns {
      // Nothing of this batch moved for its budget: what came back is kept, the rest is given up (`Held`'s drop).
      return given_up(peer, &held, begun);
    }
    let left_ns = stall_ns.saturating_sub(quiet_ns);
    if held.driving {
      if let Some(ended) = drive_turn(
        peer,
        left_ns,
        &held,
        (&mut begun, &mut first_reply, &mut missed),
      )
      .await
      {
        return ended;
      }
    } else {
      // Woken when one of this batch's replies is taken, or when the driver leaves; bounded by the budget left.
      let _ = futures::within(left_ns, wait_woken(peer, forwarder)).await;
      begun = begun || any_begun(peer, &held.tickets);
      if first_reply.is_none() {
        first_reply = first_answered(peer, &held.tickets);
      }
    }
  }
}

/// One driving turn of a forwarder ([`forward_batch`]): `Some` with what the forwarder returns when the batch ends on
/// it (its session failed, or the shard is stopping), else `None` to look again.
async fn drive_turn(
  peer: HostId,
  left_ns: u64,
  held: &Held,
  (begun, first_reply, missed): (&mut bool, &mut Option<(u64, usize)>, &mut bool),
) -> Option<Option<BatchReplies>> {
  match turn(peer, left_ns).await {
    Turn::Driven { any_reply } => {
      *begun = true;
      if any_reply && first_reply.is_none() {
        *first_reply = first_answered(peer, &held.tickets);
      }
      // A turn's drive returns Ready whenever a packet is already there, so under a burst of replies the turns
      // never wait: each turn ends the poll's slice, and the shard steps between them (the bounded-work rule).
      futures::yield_now().await;
    }
    Turn::SessionOut => {
      count_miss(peer, missed);
      if futures::sleep(crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD)
        .await
        .is_err()
      {
        return Some(None);
      }
    }
    // The session failed under the turn: what came back is kept, the rest answered unanswered, as a batch cut off
    // by its session's loss always was.
    Turn::Failed => return Some(given_up(peer, held, true)),
  }
  None
}

/// The batch's replies so far, its budget spent: `None` when none of it was ever begun (the session never came),
/// else the replies with `None` for those not answered.
fn given_up(peer: HostId, held: &Held, begun: bool) -> Option<BatchReplies> {
  state::with_state(|s| s.count(BATCH_GIVEN_UP, 1));
  let replies: Vec<Option<Vec<u8>>> = state::with_state(|s| {
    let lane = s.forward_lanes.lanes.get(&peer);
    held
      .tickets
      .iter()
      .map(
        |ticket| match lane.and_then(|lane| lane.tickets.get(ticket)) {
          Some(Ticket {
            stage: Stage::Done(reply),
            ..
          }) => reply.clone(),
          _ => None,
        },
      )
      .collect()
  })
  .unwrap_or_default();
  if !begun && replies.iter().all(Option::is_none) {
    state::with_state(|s| s.count("fleet.forward.session_never_returned", 1));
    return None;
  }
  Some(BatchReplies {
    replies,
    streamed_bytes: 0,
    streamed_ns: 0,
  })
}

/// The first of `tickets` answered, with when and its length: the start of the batch's streamed figures.
fn first_answered(peer: HostId, tickets: &[u64]) -> Option<(u64, usize)> {
  state::with_state(|s| {
    let lane = s.forward_lanes.lanes.get(&peer)?;
    tickets
      .iter()
      .find_map(|ticket| match lane.tickets.get(ticket) {
        Some(Ticket {
          stage: Stage::Done(Some(reply)),
          ..
        }) => Some((futures::now_ns(), reply.len())),
        _ => None,
      })
  })
  .flatten()
}

/// Whether any of `tickets` has been begun on the session (or answered).
fn any_begun(peer: HostId, tickets: &[u64]) -> bool {
  state::with_state(|s| {
    s.forward_lanes.lanes.get(&peer).is_some_and(|lane| {
      tickets.iter().any(|ticket| {
        lane
          .tickets
          .get(ticket)
          .is_some_and(|t| !matches!(t.stage, Stage::Queued))
      })
    })
  })
  .unwrap_or(false)
}

/// Counts a turn that found the session out, by where it was, once per forwarder (as the one-at-a-time forward did).
fn count_miss(peer: HostId, missed: &mut bool) {
  if std::mem::replace(missed, true) {
    return;
  }
  state::with_state(|s| {
    let link = s.record_sessions.get(&peer);
    let missing = if link.is_some() {
      "fleet.forward.session_out"
    } else {
      "fleet.forward.no_session"
    };
    let holder = link.map(|link| link.lent_to.unwrap_or(crate::fleet::LENT_TO_LINK));
    s.count(missing, 1);
    if let Some(holder) = holder {
      s.count(holder, 1);
    }
  });
}

/// Waits until `forwarder` is woken (one of its replies taken, or the lane left undriven).
async fn wait_woken(peer: HostId, forwarder: u64) {
  let mut registered = false;
  std::future::poll_fn(|cx| {
    if registered {
      return Poll::Ready(());
    }
    registered = true;
    let kept = state::with_state(|s| {
      s.forward_lanes
        .lanes
        .get_mut(&peer)
        .map(|lane| lane.waiters.insert(forwarder, cx.waker().clone()))
        .is_some()
    })
    .unwrap_or(false);
    if kept { Poll::Pending } else { Poll::Ready(()) }
  })
  .await;
}

/// How a driving turn went.
enum Turn {
  /// The session was borrowed and driven; whether any reply was taken.
  Driven { any_reply: bool },
  /// The session was out (lent to another borrower) or there is no link.
  SessionOut,
  /// The session failed under the turn, or the turn could not be timed.
  Failed,
}

/// One driving turn of `peer`'s lane, within `left_ns`: borrow the session, settle the lane against it, drive it
/// until a packet, a timer or a newly queued forward, settle again, and return it.
async fn turn(peer: HostId, left_ns: u64) -> Turn {
  let Some((_, mut endpoint)) =
    crate::fleet::take_sessions("fleet.lent_to.forward_lane", |host| host == peer).pop()
  else {
    return Turn::SessionOut;
  };
  let session = endpoint.connection_id().ok();
  apply_injected_deafness(peer, &mut endpoint);
  let mut any_reply = settle(peer, &mut endpoint, session);
  let consumed = endpoint.bytes_consumed();
  let driven = futures::within(left_ns, drive_or_queued(peer, &mut endpoint)).await;
  let progressed = endpoint.bytes_consumed() != consumed;
  any_reply |= settle(peer, &mut endpoint, session);
  if progressed {
    mark_progress(peer, &mut endpoint, session);
  }
  crate::fleet::return_sessions(vec![(peer, endpoint)]);
  match driven {
    Ok(Some(Err(_))) | Err(_) => Turn::Failed,
    Ok(Some(Ok(()))) | Ok(None) => Turn::Driven { any_reply },
  }
}

/// Drives `endpoint` one turn, or returns as soon as a forward is queued on `peer`'s lane (the turn is cancel-safe:
/// `Endpoint::drive` loses nothing when dropped while it waits).
async fn drive_or_queued(peer: HostId, endpoint: &mut Endpoint) -> Result<(), EndpointError> {
  let mut drive = std::pin::pin!(endpoint.drive());
  std::future::poll_fn(|cx| {
    if let Poll::Ready(driven) = drive.as_mut().poll(cx) {
      return Poll::Ready(driven);
    }
    let queued = state::with_state(|s| {
      let lane = s.forward_lanes.lanes.get_mut(&peer)?;
      if lane.fresh_queued {
        Some(true)
      } else {
        lane.driver = Some(cx.waker().clone());
        Some(false)
      }
    })
    .flatten()
    .unwrap_or(false);
    if queued {
      Poll::Ready(Ok(()))
    } else {
      Poll::Pending
    }
  })
  .await
}

/// Settles `peer`'s lane against the borrowed session: abandons what forwarders gave up, answers unanswered every
/// forward begun on a session since replaced, takes the replies that completed, begins every queued forward the
/// session's stream credit admits, and wakes the forwarders answered. Its cost is what changed (replies completed,
/// forwards begun), not the forwards held. Whether any reply was taken.
fn settle(peer: HostId, endpoint: &mut Endpoint, session: Option<ConnectionId>) -> bool {
  let counts = state::with_state(|s| {
    let lane = s.forward_lanes.lanes.get_mut(&peer)?;
    abandon_given_up(lane, endpoint, session);
    let mut answered = Answered::default();
    let replaced = answer_open(lane, endpoint, session, &mut answered);
    let begun = session.map_or(0, |on| begin_queued(lane, endpoint, on, &mut answered));
    let woken = wake_answered(lane, &answered.forwarders);
    if answered.any_reply
      && let Some(dropped) = s.dead_session_dropped_at.remove(&peer)
    {
      let recovered = futures::now_ns().saturating_sub(dropped);
      s.dead_session_recovered_ns = Some(recovered);
      s.count(
        if recovered <= crate::daemon::LIVENESS_BUDGET_NS {
          REPLACED_ANSWERED_IN_BUDGET
        } else {
          REPLACED_ANSWERED_PAST_BUDGET
        },
        1,
      );
      slates_machine::log_line!(
        "slates-server: forward lane to {peer:?}: first answer {} ms after its silent session was dropped",
        recovered / NS_PER_MS
      );
    }
    Some((answered.any_reply, replaced, begun, woken))
  })
  .flatten();
  let Some((any, replaced, begun, woken)) = counts else {
    return false;
  };
  state::with_state(|s| {
    s.count(SESSION_REPLACED, replaced);
    s.count(BEGUN, begun);
    s.count(TAKEN_FOR_ANOTHER, woken);
  });
  any
}

/// The forwards one settle answered: their forwarders, to wake, and whether any was a reply.
#[derive(Default)]
struct Answered {
  forwarders: Vec<u64>,
  any_reply: bool,
}

/// Abandons, on the session they were begun on, the exchanges whose forwarders gave up.
fn abandon_given_up(lane: &mut Lane, endpoint: &mut Endpoint, session: Option<ConnectionId>) {
  for (exchange, on) in std::mem::take(&mut lane.abandon) {
    if Some(on) == session {
      endpoint.abandon(exchange);
    }
  }
}

/// Takes the replies of the open forwards that completed; every forward open on a session since replaced is answered
/// unanswered, once, when the replacement is first seen. How many were replaced.
fn answer_open(
  lane: &mut Lane,
  endpoint: &mut Endpoint,
  session: Option<ConnectionId>,
  answered: &mut Answered,
) -> u64 {
  let now = futures::now_ns();
  let mut replaced = 0u64;
  if lane.open_on != session {
    for ticket in std::mem::take(&mut lane.open).into_values() {
      if let Some(entry) = lane.tickets.get_mut(&ticket) {
        entry.stage = Stage::Done(None);
        answered.forwarders.push(entry.forwarder);
        replaced = replaced.saturating_add(1);
      }
    }
    lane.open_on = session;
  }
  for exchange in endpoint.take_completed() {
    let Some(ticket) = lane.open.get(&exchange).copied() else {
      continue; // Another borrower's exchange on the shared session.
    };
    let Some(reply) = endpoint.take_reply(exchange) else {
      continue;
    };
    lane.open.remove(&exchange);
    if let Some(entry) = lane.tickets.get_mut(&ticket) {
      entry.stage = Stage::Done(Some(reply));
      entry.progress_ns = now;
      answered.forwarders.push(entry.forwarder);
      answered.any_reply = true;
    }
  }
  replaced
}

/// Test support: makes `endpoint` deaf if a deafness is injected for `peer`, once, so the session the link dials to
/// replace it is healthy (`Daemon::inject_session_deafness`). In the lane's own borrow, so the session is deaf in the
/// lane's hands whichever turn takes it. Nothing is injected in production.
fn apply_injected_deafness(peer: HostId, endpoint: &mut Endpoint) {
  let injected = state::with_state(|s| {
    let hit = s.injected_deaf_session == Some(peer);
    if hit {
      s.injected_deaf_session = None;
    }
    hit
  });
  if injected == Some(true) {
    endpoint.set_deaf();
  }
}

/// Begins queued forwards on session `on`, in their places' order ([`Place`]), while its stream credit admits them (a
/// refusal past the credit, `Backlogged`, leaves the rest queued for a stream to free; any other refusal answers that
/// forward unanswered). How many were begun.
fn begin_queued(
  lane: &mut Lane,
  endpoint: &mut Endpoint,
  on: ConnectionId,
  answered: &mut Answered,
) -> u64 {
  begin_queued_with(lane, on, futures::now_ns(), answered, |request| {
    endpoint.begin(crate::fleet::FORWARD_STREAM, Priority::Metadata, request)
  })
}

/// [`begin_queued`] with the session's begin as `begin`, at `now`: the queue's discipline apart from the session.
fn begin_queued_with(
  lane: &mut Lane,
  on: ConnectionId,
  now: u64,
  answered: &mut Answered,
  mut begin: impl FnMut(&[u8]) -> Result<u64, EndpointError>,
) -> u64 {
  let mut begun = 0u64;
  lane.fresh_queued = false;
  while let Some(queued) = lane.queued.first().copied() {
    let (_, ticket) = queued;
    let Some(entry) = lane.tickets.get_mut(&ticket) else {
      lane.queued.remove(&queued);
      continue;
    };
    match begin(&entry.request) {
      Ok(exchange) => {
        entry.stage = Stage::Open {
          exchange,
          session: on,
        };
        entry.progress_ns = now;
        lane.open.insert(exchange, ticket);
        begun = begun.saturating_add(1);
      }
      Err(EndpointError::Stream(slates_transport::streams::StreamRefusal::Backlogged {
        ..
      })) => break,
      Err(_) => {
        entry.stage = Stage::Done(None);
        answered.forwarders.push(entry.forwarder);
      }
    }
    lane.queued.remove(&queued);
  }
  begun
}

/// Wakes the waiting forwarders this settle answered: how many were woken. A forwarder looks at its tickets and
/// registers its wait in one poll, so one answered before it waited sees its answer without a wake.
fn wake_answered(lane: &mut Lane, forwarders: &[u64]) -> u64 {
  let mut woken = 0u64;
  for forwarder in forwarders {
    if let Some(waker) = lane.waiters.remove(forwarder) {
      woken = woken.saturating_add(1);
      waker.wake();
    }
  }
  woken
}

/// Marks each open forward whose own reply read bytes since the last turn as having made progress now, as the
/// session reports them (`Endpoint::take_progressed`). Bytes the session consumed for other exchanges are not this
/// forward's progress.
fn mark_progress(peer: HostId, endpoint: &mut Endpoint, session: Option<ConnectionId>) {
  let now = futures::now_ns();
  let _ = state::with_state(|s| {
    let Some(lane) = s.forward_lanes.lanes.get_mut(&peer) else {
      return;
    };
    if lane.open_on != session {
      return;
    }
    for exchange in endpoint.take_progressed() {
      if let Some(entry) = lane
        .open
        .get(&exchange)
        .and_then(|ticket| lane.tickets.get_mut(ticket))
      {
        entry.progress_ns = now;
      }
    }
  });
}

/// What a peer's serve loop takes next ([`next_request_or_forwarded`]).
pub(crate) enum Next {
  /// A request the peer sent on `stream_id`, of `kind`.
  Request {
    stream_id: u64,
    kind: u64,
    request: Vec<u8>,
  },
  /// A forwarded verb served on its own task, its reply due on `stream_id`.
  Forwarded { stream_id: u64, reply: Vec<u8> },
}

/// The forwarded verbs one peer's serve loop is running, each on its own child task of the loop, so one waiting on its
/// owner shard holds no other request of that peer (a record commit, a prepare, or another forward). Bounded by the
/// peer's stream credit on the session: a peer opens no more requests than its credit admits, and each child answers
/// one. Dropped with the loop, its slot goes; the children are the loop task's own, cancelled and joined with it.
pub(crate) struct ServedForwards {
  slot: u64,
  running: BTreeMap<u64, slates_rt::TaskId>,
}

/// One serve loop's finished forwards, kept in the control shard's state for the loop to reply to.
#[derive(Default)]
struct ServeSlot {
  done: VecDeque<(u64, Vec<u8>)>,
  waker: Option<Waker>,
}

/// The status count of a forwarded verb answered empty because its task could not be spawned (the origin's forward is
/// then unanswered, and its client's retry meets the completion record or runs it).
const SERVE_SPAWN_REFUSED: &str = "fleet.forward.serve_spawn_refused";

impl ServedForwards {
  /// A serve loop's forwards, registered in the shard's state.
  pub(crate) fn new() -> ServedForwards {
    let slot = state::with_state(|s| {
      let lanes = &mut s.forward_lanes;
      lanes.next_serve = lanes.next_serve.saturating_add(1);
      lanes.served.insert(lanes.next_serve, ServeSlot::default());
      lanes.next_serve
    })
    .unwrap_or(0);
    ServedForwards {
      slot,
      running: BTreeMap::new(),
    }
  }

  /// Runs `serve` for the request on `stream_id` as a child task of the loop. `Some(reply)` to send at once when the
  /// task could not be spawned (an empty reply, counted).
  pub(crate) fn spawn(
    &mut self,
    stream_id: u64,
    serve: impl core::future::Future<Output = Vec<u8>> + 'static,
  ) -> Option<Vec<u8>> {
    let slot = self.slot;
    let spawned = futures::spawn_child(async move {
      let reply = serve.await;
      let _ = state::with_state(|s| {
        if let Some(served) = s.forward_lanes.served.get_mut(&slot) {
          served.done.push_back((stream_id, reply));
          if let Some(waker) = served.waker.take() {
            waker.wake();
          }
        }
      });
    });
    match spawned {
      Ok(task) => {
        self.running.insert(stream_id, task);
        None
      }
      Err(_) => {
        state::with_state(|s| s.count(SERVE_SPAWN_REFUSED, 1));
        Some(Vec::new())
      }
    }
  }

  /// A finished forward's reply, taken; when none is finished, `waker` is kept so the next one wakes the loop.
  fn take_done(&self, waker: Option<&Waker>) -> Option<(u64, Vec<u8>)> {
    state::with_state(|s| {
      let served = s.forward_lanes.served.get_mut(&self.slot)?;
      let done = served.done.pop_front();
      if done.is_none()
        && let Some(waker) = waker
      {
        served.waker = Some(waker.clone());
      }
      done
    })
    .flatten()
  }
}

impl Drop for ServedForwards {
  fn drop(&mut self) {
    let _ = state::with_state(|s| s.forward_lanes.served.remove(&self.slot));
  }
}

/// The next request the peer sent, or the next forwarded verb finished on its task, whichever comes first.
/// `Endpoint::next_request` is cancel-safe (a request is taken from the session only when it is returned), so racing
/// it loses nothing. A finished forward's task is joined here, so its arena slot is released as it answers.
pub(crate) async fn next_request_or_forwarded(
  endpoint: &mut Endpoint,
  forwards: &mut ServedForwards,
) -> Result<Next, EndpointError> {
  let next = {
    let mut request = std::pin::pin!(endpoint.next_request());
    let forwards: &ServedForwards = forwards;
    std::future::poll_fn(|cx| {
      if let Some((stream_id, reply)) = forwards.take_done(Some(cx.waker())) {
        return Poll::Ready(Ok(Next::Forwarded { stream_id, reply }));
      }
      request.as_mut().poll(cx).map(|taken| {
        taken.map(|(stream_id, kind, request)| Next::Request {
          stream_id,
          kind,
          request,
        })
      })
    })
    .await
  };
  if let Ok(Next::Forwarded { stream_id, .. }) = &next
    && let Some(task) = forwards.running.remove(stream_id)
  {
    let _ = futures::join(task).await;
  }
  next
}

#[cfg(test)]
mod tests {
  use std::collections::{BTreeMap, BTreeSet};

  use proptest::prelude::*;
  use slates_db::HostId;
  use slates_transport::endpoint::{CONNECTION_ID_BYTES, ConnectionId, EndpointError};
  use slates_transport::streams::StreamRefusal;

  use super::{Answered, ForwardLanes, Place, begin_queued_with};

  /// Shape: the owner every test's lane forwards to.
  const PEER: HostId = HostId(1);
  /// Shape: the session every test settles against (its id only names it).
  const SESSION: ConnectionId = [7; CONNECTION_ID_BYTES];
  /// Shape: a lane bound no test reaches, so the bound never decides an order.
  const BOUND: usize = 1 << 16;

  /// Begins up to `credit` of the lane's queued forwards on one session, as a settle does when the session's stream
  /// credit admits that many and refuses the next `Backlogged`, numbering the exchanges on from `exchanges`: the
  /// requests begun, in the order they went out.
  fn begin(lanes: &mut ForwardLanes, credit: usize, exchanges: &mut u64) -> Vec<Vec<u8>> {
    let mut sent: Vec<Vec<u8>> = Vec::new();
    let Some(lane) = lanes.lanes.get_mut(&PEER) else {
      return sent;
    };
    let mut answered = Answered::default();
    begin_queued_with(lane, SESSION, 0, &mut answered, |request| {
      if sent.len() >= credit {
        return Err(EndpointError::Stream(StreamRefusal::Backlogged {
          waiting: 0,
          limit: 0,
        }));
      }
      sent.push(request.to_vec());
      *exchanges += 1;
      Ok(*exchanges)
    });
    sent
  }

  /// A-306: do queue a forward P on a lane, then three forwards first queued after it; give P up, as its forwarder does
  /// at its stall budget, and resend it at the place it kept; then begin the lane's forwards one at a time, as a
  /// session's stream credit admits them. Expect P begun first, then the three in the order they were queued. Before, a
  /// resend took fresh tickets at the back of the lane, and P went out after every forward first queued after it: a
  /// read through a client node whose session to its owner died was refused `OwnerUnreachable` at its failover bound,
  /// given up again and again behind a burst's backlog (the bug record of 2026-11-09, finding (b)).
  #[test]
  fn a_resend_is_begun_ahead_of_every_forward_first_queued_after_it() {
    let mut lanes = ForwardLanes::default();
    let mut kept = Place::default();
    let (p, p_tickets) = lanes
      .queue(PEER, &[b"p".to_vec()], &mut kept, BOUND, 0)
      .unwrap();
    for name in [b"q1", b"q2", b"q3"] {
      lanes
        .queue(PEER, &[name.to_vec()], &mut Place::default(), BOUND, 0)
        .unwrap();
    }
    lanes.leave(PEER, p, &p_tickets, false);
    lanes
      .queue(PEER, &[b"p".to_vec()], &mut kept, BOUND, 0)
      .unwrap();
    let mut exchanges = 0;
    let order: Vec<Vec<u8>> = (0..4)
      .flat_map(|_| begin(&mut lanes, 1, &mut exchanges))
      .collect();
    assert_eq!(
      order,
      [
        b"p".to_vec(),
        b"q1".to_vec(),
        b"q2".to_vec(),
        b"q3".to_vec()
      ],
      "the resent P goes out at the place its first issue had"
    );
  }

  /// One step of a lane's history in `a_lanes_forwards_are_begun_in_the_order_they_were_first_queued`.
  #[derive(Clone, Debug)]
  enum Step {
    /// A new forwarder queues a batch of this many requests, its first issue.
    Issue(usize),
    /// The forwarder at this index among those queued now gives up its batch (its tickets leave, begun or not).
    GiveUp(usize),
    /// The forwarder at this index among those given up resends its batch at the place it kept.
    Resend(usize),
    /// A settle begins up to this many queued forwards.
    Begin(usize),
  }

  fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
      (1usize..4).prop_map(Step::Issue),
      (0usize..16).prop_map(Step::GiveUp),
      (0usize..16).prop_map(Step::Resend),
      (0usize..5).prop_map(Step::Begin),
    ]
  }

  /// One logical forwarder of the model: its batch's requests and the place the lane keeps for it.
  struct Batch {
    requests: Vec<Vec<u8>>,
    place: Place,
    /// The lane's forwarder id and tickets while it is queued.
    held: Option<(u64, Vec<u64>)>,
  }

  /// A lane driven beside the model of its rule (A-306): a lane begins its queued forwards in the order of their first
  /// issue, a batch's requests in their order, however often each was given up and resent.
  #[derive(Default)]
  struct Run {
    lanes: ForwardLanes,
    batches: Vec<Batch>,
    /// The model: each request queued now, by (first-issue order, offset in its batch).
    model: BTreeSet<(usize, usize)>,
    /// The batch and offset each request's bytes name, to read a begun request back into the model's terms.
    named: BTreeMap<Vec<u8>, (usize, usize)>,
    /// The exchanges numbered so far on the session.
    exchanges: u64,
  }

  impl Run {
    fn step(&mut self, step: &Step) {
      match *step {
        Step::Issue(count) => self.issue(count),
        Step::GiveUp(pick) => self.give_up(pick),
        Step::Resend(pick) => self.resend(pick),
        Step::Begin(credit) => self.begin(credit),
      }
    }

    fn issue(&mut self, count: usize) {
      let index = self.batches.len();
      let requests: Vec<Vec<u8>> = (0..count)
        .map(|offset| format!("{index}.{offset}").into_bytes())
        .collect();
      for (offset, request) in requests.iter().enumerate() {
        self.named.insert(request.clone(), (index, offset));
        self.model.insert((index, offset));
      }
      let mut place = Place::default();
      let held = self.lanes.queue(PEER, &requests, &mut place, BOUND, 0);
      assert!(held.is_some(), "a lane under its bound queues a batch");
      self.batches.push(Batch {
        requests,
        place,
        held,
      });
    }

    /// The batch `pick` names among those in the lane (`queued`) or among those given up (not `queued`).
    fn pick(&self, queued: bool, pick: usize) -> Option<usize> {
      let matching: Vec<usize> = (0..self.batches.len())
        .filter(|index| self.batches[*index].held.is_some() == queued)
        .collect();
      (!matching.is_empty()).then(|| matching[pick % matching.len()])
    }

    fn give_up(&mut self, pick: usize) {
      let Some(index) = self.pick(true, pick) else {
        return;
      };
      if let Some((forwarder, tickets)) = self.batches[index].held.take() {
        self.lanes.leave(PEER, forwarder, &tickets, false);
      }
      self.model.retain(|(batch, _)| *batch != index);
    }

    fn resend(&mut self, pick: usize) {
      let Some(index) = self.pick(false, pick) else {
        return;
      };
      let batch = &mut self.batches[index];
      batch.held = self
        .lanes
        .queue(PEER, &batch.requests, &mut batch.place, BOUND, 0);
      assert!(
        batch.held.is_some(),
        "a lane under its bound queues a resend"
      );
      for offset in 0..batch.requests.len() {
        self.model.insert((index, offset));
      }
    }

    fn begin(&mut self, credit: usize) {
      let begun: Vec<(usize, usize)> = begin(&mut self.lanes, credit, &mut self.exchanges)
        .iter()
        .map(|request| self.named[request])
        .collect();
      let expected: Vec<(usize, usize)> = self.model.iter().take(credit).copied().collect();
      assert_eq!(
        begun, expected,
        "a settle begins the queued forwards first issued earliest"
      );
      for request in &begun {
        self.model.remove(request);
      }
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// A-306, against a model: do run generated histories on one lane (batches first issued, given up whether queued or
    /// begun, resent at their kept places, and settles that begin as many as a stream credit admits). Expect every
    /// settle to begin exactly the queued forwards the model names: those first issued earliest, a batch's in its
    /// order.
    #[test]
    fn a_lanes_forwards_are_begun_in_the_order_they_were_first_queued(
      steps in proptest::collection::vec(step(), 0..64),
    ) {
      let mut run = Run::default();
      for step in &steps {
        run.step(step);
      }
    }
  }
}
