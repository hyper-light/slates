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
//! forward is queued (a queued forward wakes the driver, so it never waits for the open ones' silence), and returns
//! the session. Between turns the session is back in its slot, so record commits and discovery pages borrow it as
//! before. A forwarder whose replies have all come stops driving and wakes the next waiter. An exchange left open
//! is taken by whichever forwarder drives next.
//!
//! **Bounds.** A lane holds at most [`lane_bound`] forwards (queued and open together): the requests this node's
//! clients may hold in flight, `clients_per_shard × slots` on each shard. Past it a forward is refused at once
//! (`fleet.forward.lane_full`), never queued. The session's own stream credit bounds the open exchanges, and a
//! forward past it waits queued (`Backlogged`, refused typed by the endpoint) until a stream frees. Each forwarder
//! bounds its own wait: the batch is given up once none of its forwards has made progress for its stall budget,
//! where progress is a reply completed or bytes consumed on the session while one of them was open. Its open
//! exchanges are abandoned on the next turn and its queued ones dropped.
//!
//! **What a session change does.** An exchange is tied to the session it was begun on (its connection id). If the
//! link replaced the session meanwhile, the forward is answered as unanswered, as a forward cut off by its session's
//! loss always was, and the caller's refusal and retry path takes it from there.

use std::collections::{BTreeMap, VecDeque};
use std::task::{Poll, Waker};

use slates_cluster::BatchReplies;
use slates_db::HostId;
use slates_machine::derived;
use slates_rt::futures;
use slates_transport::connection::Priority;
use slates_transport::endpoint::{ConnectionId, Endpoint, EndpointError};

use crate::state;

/// The status count of a forward refused because its lane held [`lane_bound`] forwards already.
const LANE_FULL: &str = "fleet.forward.lane_full";
/// The status count of a forward answered unanswered because its session was replaced while it was open.
const SESSION_REPLACED: &str = "fleet.forward.session_replaced";
/// The status count of a forward begun on the session (the non-vacuity count of the multiplexed path).
const BEGUN: &str = "fleet.forward.begun";
/// The status count of a waiting forwarder woken by the driver with its reply taken: a reply begun on the session
/// and taken by another forwarder's turn (the count that shows forwards sharing the session rather than taking turns).
const TAKEN_FOR_ANOTHER: &str = "fleet.forward.taken_for_another";

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
  /// When it last made progress: queued, begun, or bytes consumed on its session while it was open.
  progress_ns: u64,
}

/// One peer's forwards.
#[derive(Default)]
struct Lane {
  /// Forwards waiting to be begun, in the order they were queued.
  queued: VecDeque<u64>,
  tickets: BTreeMap<u64, Ticket>,
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
  derived!(
    state
      .config
      .clients_per_shard
      .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(usize::MAX))
      .saturating_mul(state.shards.len().max(1))
      .max(1),
    "clients_per_shard × slots × shards",
    ["clients_per_shard", "region.slots", "shards"]
  )
  .get()
}

/// Queues `requests` on `peer`'s lane under a new forwarder: its id and its tickets, in order, or `None` when the
/// lane is at its bound (counted) or the state is out of reach.
fn enqueue(peer: HostId, requests: &[Vec<u8>]) -> Option<(u64, Vec<u64>)> {
  state::with_state(|s| {
    let bound = lane_bound(s);
    let held = s
      .forward_lanes
      .lanes
      .get(&peer)
      .map_or(0, |lane| lane.tickets.len());
    if held.saturating_add(requests.len()) > bound {
      s.count(LANE_FULL, 1);
      return None;
    }
    let now = futures::now_ns();
    let lanes = &mut s.forward_lanes;
    lanes.next_forwarder = lanes.next_forwarder.saturating_add(1);
    let forwarder = lanes.next_forwarder;
    let mut tickets = Vec::with_capacity(requests.len());
    for _ in requests {
      lanes.next_ticket = lanes.next_ticket.saturating_add(1);
      tickets.push(lanes.next_ticket);
    }
    let lane = lanes.lanes.entry(peer).or_default();
    for (ticket, request) in tickets.iter().zip(requests) {
      lane.tickets.insert(
        *ticket,
        Ticket {
          forwarder,
          request: request.clone(),
          stage: Stage::Queued,
          progress_ns: now,
        },
      );
      lane.queued.push_back(*ticket);
    }
    if let Some(driver) = lane.driver.take() {
      driver.wake();
    }
    Some((forwarder, tickets))
  })
  .flatten()
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

/// Removes `forwarder`'s tickets from `peer`'s lane: an open one's exchange is abandoned on the next turn, and if it
/// was driving, the lane is left undriven and the next waiter woken to drive it.
fn leave(peer: HostId, forwarder: u64, tickets: &[u64], was_driving: bool) {
  let _ = state::with_state(|s| {
    let Some(lane) = s.forward_lanes.lanes.get_mut(&peer) else {
      return;
    };
    for ticket in tickets {
      if let Some(Ticket {
        stage: Stage::Open { exchange, session },
        ..
      }) = lane.tickets.remove(ticket)
      {
        lane.abandon.push((exchange, session));
      }
    }
    lane
      .queued
      .retain(|ticket| lane.tickets.contains_key(ticket));
    lane.waiters.remove(&forwarder);
    if was_driving {
      lane.driving = false;
      lane.driver = None;
      if let Some(next) = lane.waiters.values().next() {
        next.wake_by_ref();
      }
    }
    if lane.tickets.is_empty() && lane.abandon.is_empty() && !lane.driving {
      s.forward_lanes.lanes.remove(&peer);
    }
  });
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
/// the first, and the time from the first reply to the last (the session's bytes are no longer one batch's). Called
/// from the control shard, where the record sessions live.
pub(crate) async fn forward_batch(
  peer: HostId,
  requests: &[Vec<u8>],
  stall_ns: u64,
) -> Option<BatchReplies> {
  let (forwarder, tickets) = enqueue(peer, requests)?;
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
        let last_ns = futures::now_ns();
        let bytes: usize = replies.iter().flatten().map(Vec::len).sum();
        let (first_ns, first_len) = first_reply.unwrap_or((last_ns, bytes));
        return Some(BatchReplies {
          // A forward lane's tickets are all sent: its caller reads the replies, not how many began.
          begun: replies.len(),
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
      match turn(peer, left_ns).await {
        Turn::Driven { any_reply } => {
          begun = true;
          if any_reply && first_reply.is_none() {
            first_reply = first_answered(peer, &held.tickets);
          }
        }
        Turn::SessionOut => {
          count_miss(peer, &mut missed);
          if futures::sleep(crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD)
            .await
            .is_err()
          {
            return None;
          }
        }
        // The session failed under the turn: what came back is kept, the rest answered unanswered, as a batch cut off
        // by its session's loss always was.
        Turn::Failed => return given_up(peer, &held, true),
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

/// The batch's replies so far, its budget spent: `None` when none of it was ever begun (the session never came),
/// else the replies with `None` for those not answered.
fn given_up(peer: HostId, held: &Held, begun: bool) -> Option<BatchReplies> {
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
    begun: replies.len(),
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
  let mut any_reply = settle(peer, &mut endpoint, session);
  let consumed = endpoint.bytes_consumed();
  let driven = futures::within(left_ns, drive_or_queued(peer, &mut endpoint)).await;
  let progressed = endpoint.bytes_consumed() != consumed;
  any_reply |= settle(peer, &mut endpoint, session);
  if progressed {
    mark_progress(peer, session);
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
      if lane.queued.is_empty() {
        lane.driver = Some(cx.waker().clone());
        Some(false)
      } else {
        Some(true)
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
/// forward begun on a session since replaced, begins every queued forward the session's stream credit admits, and
/// takes every reply that has arrived, waking its forwarder. Whether any reply was taken.
fn settle(peer: HostId, endpoint: &mut Endpoint, session: Option<ConnectionId>) -> bool {
  let counts = state::with_state(|s| {
    let lane = s.forward_lanes.lanes.get_mut(&peer)?;
    abandon_given_up(lane, endpoint, session);
    let replaced = answer_open(lane, endpoint, session);
    let begun = session.map_or(0, |on| begin_queued(lane, endpoint, on));
    let (any, woken) = wake_answered(lane);
    Some((any, replaced, begun, woken))
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

/// Abandons, on the session they were begun on, the exchanges whose forwarders gave up.
fn abandon_given_up(lane: &mut Lane, endpoint: &mut Endpoint, session: Option<ConnectionId>) {
  for (exchange, on) in std::mem::take(&mut lane.abandon) {
    if Some(on) == session {
      endpoint.abandon(exchange);
    }
  }
}

/// Takes each open forward's reply if it has arrived; a forward begun on a session since replaced is answered
/// unanswered. How many were replaced.
fn answer_open(lane: &mut Lane, endpoint: &mut Endpoint, session: Option<ConnectionId>) -> u64 {
  let now = futures::now_ns();
  let mut replaced = 0u64;
  for ticket in lane.tickets.values_mut() {
    let Stage::Open {
      exchange,
      session: on,
    } = ticket.stage
    else {
      continue;
    };
    if Some(on) != session {
      ticket.stage = Stage::Done(None);
      replaced = replaced.saturating_add(1);
    } else if let Some(reply) = endpoint.take_reply(exchange) {
      ticket.stage = Stage::Done(Some(reply));
      ticket.progress_ns = now;
    }
  }
  replaced
}

/// Begins queued forwards on session `on`, in order, while its stream credit admits them (a refusal past the credit,
/// `Backlogged`, leaves the rest queued for a stream to free; any other refusal answers that forward unanswered).
/// How many were begun.
fn begin_queued(lane: &mut Lane, endpoint: &mut Endpoint, on: ConnectionId) -> u64 {
  let now = futures::now_ns();
  let mut begun = 0u64;
  while let Some(ticket) = lane.queued.front().copied() {
    let Some(entry) = lane.tickets.get_mut(&ticket) else {
      lane.queued.pop_front();
      continue;
    };
    match endpoint.begin(
      crate::fleet::FORWARD_STREAM,
      Priority::Metadata,
      &entry.request,
    ) {
      Ok(exchange) => {
        entry.stage = Stage::Open {
          exchange,
          session: on,
        };
        entry.progress_ns = now;
        begun = begun.saturating_add(1);
      }
      Err(EndpointError::Stream(slates_transport::streams::StreamRefusal::Backlogged {
        ..
      })) => break,
      Err(_) => entry.stage = Stage::Done(None),
    }
    lane.queued.pop_front();
  }
  begun
}

/// Wakes every waiting forwarder with a forward answered: whether any reply was taken, and how many were woken.
fn wake_answered(lane: &mut Lane) -> (bool, u64) {
  let mut any = false;
  let mut answered = Vec::new();
  for ticket in lane.tickets.values() {
    if let Stage::Done(reply) = &ticket.stage {
      any |= reply.is_some();
      answered.push(ticket.forwarder);
    }
  }
  let mut woken = 0u64;
  for forwarder in answered {
    if let Some(waker) = lane.waiters.remove(&forwarder) {
      woken = woken.saturating_add(1);
      waker.wake();
    }
  }
  (any, woken)
}

/// Marks every forward open on `session` as having made progress now (bytes were consumed on it).
fn mark_progress(peer: HostId, session: Option<ConnectionId>) {
  let now = futures::now_ns();
  let _ = state::with_state(|s| {
    if let Some(lane) = s.forward_lanes.lanes.get_mut(&peer) {
      for ticket in lane.tickets.values_mut() {
        if matches!(ticket.stage, Stage::Open { session: on, .. } if Some(on) == session) {
          ticket.progress_ns = now;
        }
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
