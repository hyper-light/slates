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
//! bounds its own wait: the batch is given up once none of its forwards has made progress for its stall budget
//! ([`Stall`]), where an open forward's progress is a reply completed or bytes of its own reply read, and a queued
//! one's is its lane's. Its open exchanges are abandoned on the next turn and its queued ones dropped. Until 2026-11-07
//! progress was any byte the session consumed while a forward was open: a session busy with other replies kept a
//! forward its owner never answered alive for good (298 in fixtures' 1,000-client primary-kill rung, the owner unable
//! to commit and the session full of `LeaseUnconfirmed` replies), so the origin's resend and its `OwnerUnreachable`
//! never came.
//!
//! **A silence is judged in the shard's own time (A-315).** A batch quiet for its stall budget is given up only once
//! the lane has read what came: a driving turn of the lane, begun after the silence was first seen, must have read the
//! session's own socket empty ([`left_empty`]). The record link's sessions read their own sockets, so that turn has
//! read everything that reached the socket before the silence was seen, and settled it into the lane. Before, a
//! forwarder woken by its own timer judged at once: an origin kept off its CPU past a budget woke with its owner's
//! replies unread in its socket, and its forwarders, run before the lane's driver, gave those replies' batches up to a
//! live owner. The shard's waits in its driver (A-292's receiver rule) prove nothing here: the shard waits while
//! another borrower (the link's discovery page) holds the session, and a shard busy with other forwards' replies rarely
//! waits at all. A lane whose peer has no session at all has nothing left to read, so a driving turn that finds none
//! counts as one that read the socket empty ([`note_sessionless_turn`]): a dropped session's replies are never settled
//! into the lane, and its forwards are answered unanswered by the next session. Without it a survivor's read through a
//! forward to a stopped owner, whose silent session was dropped before the forward's silence was seen, was held to its
//! failover bound (the bug record of 2026-11-18).
//!
//! A forward still queued counts its lane's progress as its own (a forward begun, a reply taken, bytes of a reply
//! read), since nothing of it has been sent: before, a backlog longer than a budget gave every forward behind it up and
//! resent it, cycling the backlog through the control shard once a budget (17,000 to 52,000 batches a run behind a
//! 256-client burst; GAPS, A-306's open item). Both are bounded by the failover bound from the forward's first send,
//! which the caller passes: the lane's progress holds a queued forward only until then, and past it a silence is given
//! up unconfirmed. The judge reads no CPU clock and keeps no poll pace, so it holds on every platform: loadtest-scale's
//! lane poll clock (`6da9670d`, never on main) credited an idle turn at most a 10 ms pace, while Windows' completion-port
//! wait keeps the system's 15.6 ms tick, so by the code an idle origin's clock ran at about two thirds of the wall
//! there; the test that wants a forward resent at its budget saw none resent, 5 of 5 on `windows-latest`
//! (`an_unanswered_forward_is_sent_again_within_the_takeover_bound`, run 44230887960).
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
/// that failed) whose replacement the lane had a reply on within [`REPLACEMENT_ANSWER_PERIODS`] of the control shard's
/// own periods: the property the forward path owes after any such drop, whoever dropped it.
const REPLACED_ANSWERED_IN_BUDGET: &str = "fleet.forward.replaced_answered_in_budget";
/// Of the same drops, those whose replacement's first reply came later than that.
const REPLACED_ANSWERED_PAST_BUDGET: &str = "fleet.forward.replaced_answered_past_budget";
/// Derived: the coordinator periods within which the lane owes its first answer on a dropped session's replacement: the
/// link dials again at its next pace (one period, `HEARTBEAT_NS`), and a forward begun on the new session is answered
/// within its stall budget (a liveness budget, `LIVENESS_BUDGET_NS`), both counted in the control shard's own periods
/// rather than on the wall, so a daemon kept off its CPUs is judged by what it did (A-228). On the wall the same bound
/// read 1.1 s, which every recovery missed at nice 19 beside two spinners a CPU (2.3 to 13.7 s, 20 of 20 runs, the
/// integrator's diagnostics run 44274886122) while no live session was dropped and no forward given up.
const REPLACEMENT_ANSWER_PERIODS: u64 = (crate::daemon::LIVENESS_BUDGET_NS
  + crate::daemon::HEARTBEAT_NS)
  .div_ceil(crate::daemon::HEARTBEAT_NS);
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
/// Of [`BATCH_GIVEN_UP`], the batches given up silent for their stall budget once their receiver had drained
/// ([`GaveUp::Silent`]).
const GIVEN_UP_SILENT: &str = "fleet.forward.given_up_silent";
/// Of [`BATCH_GIVEN_UP`], the batches given up silent for their stall budget past their failover bound, unconfirmed
/// ([`GaveUp::AtTheBound`]).
const GIVEN_UP_AT_BOUND: &str = "fleet.forward.given_up_at_bound";
/// The status count of a batch whose silence first waited for its receiver to drain ([`Stall`]): the non-vacuity count
/// of the confirmation, so a test of a held origin shows its silences were held for the receiver, not never reached.
const SILENCE_AWAITED_RECEIVER: &str = "fleet.forward.silence_awaited_receiver";
/// The status count of a driving turn that found its peer with no session at all, counted as a turn that read the
/// session's socket empty ([`note_sessionless_turn`]): the non-vacuity count of that proof.
const SESSIONLESS_TURN: &str = "fleet.forward.sessionless_turn";
/// The status count of a lane whose path's measured floor passed the failover SLO ([`note_floor`]), once a crossing.
const FLOOR_PAST_SLO: &str = "fleet.forward.floor_past_slo";
/// The status count of a forward whose failover bound its path's floor held past the SLO ([`FailoverBound`]), once a
/// forward: the non-vacuity count of the floor, so a test of a path slower than the SLO shows the floor held it.
const HELD_PAST_SLO: &str = "fleet.forward.held_past_slo";
/// The status count of a batch with a forward still queued that its lane's progress kept past its own stall budget
/// ([`Stall`]), once a batch: the non-vacuity count of a queued forward's lane credit (A-315).
const HELD_BY_LANE: &str = "fleet.forward.held_by_lane";
/// Derived: how often a batch whose silence awaits its receiver looks again, and the longest a driving turn lasts while
/// it does: the fleet's poll interval (`HEARTBEAT_NS / POLL_PER_PERIOD`), the lane's own pace for a session that is out.
const CONFIRM_LOOK_NS: u64 = crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD;

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
  /// When the lane last moved, on the wall (0: never): a forward begun on its session, a reply taken, or bytes of a
  /// reply read. A forward still queued counts it as its own progress ([`Stall`], A-315).
  progress_ns: u64,
  /// The driving turns begun on the lane so far: each borrow of its session by its driver, and each look of its driver
  /// that found the peer with no session at all ([`note_sessionless_turn`]), counted as it begins.
  turns_begun: u64,
  /// The number ([`Lane::turns_begun`]) of the latest driving turn that read the session's own socket empty
  /// ([`left_empty`]) or found no session to read ([`note_sessionless_turn`]); 0: none yet. What a silence is
  /// confirmed by ([`Stall`]): a turn begun after the silence was seen that read the socket empty has read everything
  /// that reached it before, and settled it into the lane, and one that found no session has nothing left to read.
  drained_turn: u64,
  /// The floor of the path to the peer as the lane's latest driving turn measured it on the session it borrowed
  /// ([`path_floor_ns`]); 0 before any. Each forward holds its failover bound at the largest it saw ([`FailoverBound`]).
  floor_ns: u64,
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
  /// Not all done; whether this forwarder should drive (nobody does), what its stall is judged against, and the lane's
  /// latest measured path floor ([`Lane::floor_ns`]).
  Waiting {
    drive: bool,
    heard: Heard,
    floor_ns: u64,
  },
}

/// Looks at `tickets`; when this forwarder should drive, marks the lane driven by it.
fn look(peer: HostId, tickets: &[u64]) -> Option<Look> {
  state::with_state(|s| {
    let lane = s.forward_lanes.lanes.get_mut(&peer)?;
    Some(look_in(lane, tickets))
  })
  .flatten()
}

/// [`look`] within `lane`: the queue's view apart from the shard's state.
fn look_in(lane: &mut Lane, tickets: &[u64]) -> Look {
  let mut own_ns = 0;
  let mut queued = false;
  let mut done = Vec::with_capacity(tickets.len());
  for ticket in tickets {
    match lane
      .tickets
      .get(ticket)
      .map(|ticket| (&ticket.stage, ticket.progress_ns))
    {
      Some((Stage::Done(reply), _)) => done.push(reply.clone()),
      Some((stage, at)) => {
        own_ns = own_ns.max(at);
        queued = queued || matches!(stage, Stage::Queued);
      }
      None => done.push(None),
    }
  }
  if done.len() == tickets.len() {
    return Look::Done(done);
  }
  let drive = !lane.driving;
  if drive {
    lane.driving = true;
  }
  Look::Waiting {
    drive,
    heard: Heard {
      own_ns,
      lane_ns: if queued { lane.progress_ns } else { 0 },
      turns_begun: lane.turns_begun,
      drained_turn: lane.drained_turn,
    },
    floor_ns: lane.floor_ns,
  }
}

/// A forward's failover bound (A-315, A-352): the operator's failover SLO (`DaemonConfig::failover_slo_ns`, the takeover
/// bound `verbs::resolve_and_forward` resends within) from its first send, held longer while the path to its owner
/// measures a longer floor ([`path_floor_ns`]), the longest it has measured in the forward's life. So a forward is never
/// given up or refused below the floor of the path it waits on: where that floor is past the SLO, the SLO cannot be met
/// on that path, and the forward runs at the floor, counted ([`HELD_PAST_SLO`]) and said in the log by the lane
/// ([`note_floor`]), as A-129 runs the fleet's timers at their measured floors and reports the overrun. Before, a forward
/// through a session that went silent was refused `OwnerUnreachable` at the SLO even where the transport could not yet
/// have judged the session dead: at nice 19 beside two spinners a CPU, the starved path's silence window alone (about 9
/// s) neared the 10 s SLO, and the probe of `a_read_through_a_dead_session_keeps_its_place_behind_a_burst` was refused or
/// cut off at its bound 20 of 20 times (the integrator's diagnostics run 44274886122). Kept by the caller across its
/// resends, as its [`Place`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FailoverBound {
  /// The forward's first send, on the wall.
  first_send_ns: u64,
  /// The operator's failover SLO.
  slo_ns: u64,
  /// The longest floor the forward's path has measured; 0 before any.
  floor_ns: u64,
}

impl FailoverBound {
  /// The bound of a forward first sent at `first_send_ns` under a failover SLO of `slo_ns`.
  pub(crate) fn new(first_send_ns: u64, slo_ns: u64) -> FailoverBound {
    FailoverBound {
      first_send_ns,
      slo_ns,
      floor_ns: 0,
    }
  }

  /// The bound of a forward first sent now, under this shard's failover SLO, or one liveness budget where the shard's
  /// state is out of reach.
  pub(crate) fn from_now() -> FailoverBound {
    FailoverBound::new(
      futures::now_ns(),
      state::with_state(|s| s.config.failover_slo_ns).unwrap_or(crate::daemon::LIVENESS_BUDGET_NS),
    )
  }

  /// Holds the bound to a path floor of `floor_ns` where it is longer than any measured before: whether this is what
  /// first took the bound past the failover SLO.
  pub(crate) fn hold(&mut self, floor_ns: u64) -> bool {
    let was_past = self.floor_ns > self.slo_ns;
    self.floor_ns = self.floor_ns.max(floor_ns);
    !was_past && self.floor_ns > self.slo_ns
  }

  /// The bound on the wall: the first send plus the longer of the SLO and the floor.
  pub(crate) fn at_ns(&self) -> u64 {
    self
      .first_send_ns
      .saturating_add(self.slo_ns.max(self.floor_ns))
  }
}

/// Shape: the round trips a dropped session's replacement takes before a forward on it is answered, each a probe timeout
/// of the path: the tail a lane turn's stall budget allows past its liveness budget (`verbs::forward_once`), the
/// replacement's handshake, its first discovery page, which installs it (`fleet::enroll_record_session`), and the
/// forward's own exchange.
const FLOOR_ROUND_TRIPS: u64 = 4;

/// Derived: the floor of the path to a peer whose record session measures a persistent-congestion duration of
/// `congestion_ns` and a probe timeout of `pto_ns` (RFC 9002 §6.2.1 and §7.6.1, `Endpoint::persistent_congestion_duration`
/// and `Endpoint::pto`): the least time within which a forward on that path can be answered after its session goes
/// silent. The session is judged dead once silent for the transport's window, the longer of a liveness budget and its
/// persistent-congestion duration (`Endpoint::peer_silent_past`), at the next return of a borrower, whose hold is at most
/// a lane turn's stall budget (a liveness budget and the path's tail, the tail counted below); the link dials again at
/// its next pace (a heartbeat); and the replacement takes [`FLOOR_ROUND_TRIPS`] probe timeouts before the forward's
/// answer is read. Every term is measured or the daemon's own: on loopback, quiet, about 3.1 s (the window floored at
/// three initial probe timeouts, about 2 s); on a path whose probe timeout a starved process stretched to 3 s, about 22 s.
fn path_floor_ns(congestion_ns: u64, pto_ns: u64) -> u64 {
  crate::daemon::LIVENESS_BUDGET_NS
    .max(congestion_ns)
    .saturating_add(crate::daemon::LIVENESS_BUDGET_NS)
    .saturating_add(crate::daemon::HEARTBEAT_NS)
    .saturating_add(pto_ns.saturating_mul(FLOOR_ROUND_TRIPS))
}

/// Measures the floor of the path to `peer` on the session a driving turn borrowed ([`Lane::floor_ns`]), and says in the
/// log when it first passes the failover SLO, and when it falls back under it: while it is past, the lane's forwards run
/// at the floor and cannot meet the SLO ([`FailoverBound`]).
fn note_floor(peer: HostId, endpoint: &Endpoint) {
  let congestion_ns = endpoint.persistent_congestion_duration();
  let pto_ns = endpoint.pto();
  let floor_ns = path_floor_ns(congestion_ns, pto_ns);
  let crossed = state::with_state(|s| {
    let slo_ns = s.config.failover_slo_ns;
    let lane = s.forward_lanes.lanes.get_mut(&peer)?;
    let was_past = lane.floor_ns > slo_ns;
    lane.floor_ns = floor_ns;
    let past = floor_ns > slo_ns;
    if past && !was_past {
      s.count(FLOOR_PAST_SLO, 1);
    }
    (past != was_past).then_some((past, slo_ns))
  })
  .flatten();
  match crossed {
    Some((true, slo_ns)) => slates_machine::log_line!(
      "slates-server: forward lane to {peer:?}: the path's measured floor {} ms is past the failover SLO {} ms (its \
       silence window {} ms, its probe timeout {} ms): its forwards run at the floor and cannot meet the SLO",
      floor_ns / NS_PER_MS,
      slo_ns / NS_PER_MS,
      crate::daemon::LIVENESS_BUDGET_NS.max(congestion_ns) / NS_PER_MS,
      pto_ns / NS_PER_MS
    ),
    Some((false, slo_ns)) => slates_machine::log_line!(
      "slates-server: forward lane to {peer:?}: the path's measured floor {} ms is back within the failover SLO {} ms",
      floor_ns / NS_PER_MS,
      slo_ns / NS_PER_MS
    ),
    None => {}
  }
}

/// What a batch's stall is judged against at a look ([`Stall::judge`]): two instants on the wall and two counts, each
/// of which only grows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Heard {
  /// The latest progress of the batch's own forwards still waiting: queued, begun, or bytes of its own reply read.
  own_ns: u64,
  /// While any of its forwards is still queued, its lane's latest progress ([`Lane::progress_ns`]); 0 otherwise. Bytes
  /// of other forwards' replies are not an open forward's progress (the 298 forwards of 2026-11-07), but a queued
  /// forward has sent nothing, and its lane moving is the only progress it can make.
  lane_ns: u64,
  /// The driving turns begun on the lane so far ([`Lane::turns_begun`]).
  turns_begun: u64,
  /// The latest of them that read the session's own socket empty ([`Lane::drained_turn`]).
  drained_turn: u64,
}

/// Why a [`Stall`] gave its batch up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GaveUp {
  /// Quiet for its stall budget, and a turn of its lane begun after the quiet was first seen had read the session's own
  /// socket empty, or found the peer with no session at all.
  Silent,
  /// Quiet for its stall budget past its failover bound: given up without waiting for the receiver.
  AtTheBound,
}

/// What a [`Stall`] decided at a look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
  /// Look again by this instant on the wall, or sooner when woken.
  LookBy(u64),
  /// Give the batch up.
  GiveUp(GaveUp),
}

/// The counts a [`Stall::judge`] newly earned, each at most once an episode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Noted {
  /// The lane's progress kept the batch past its own stall budget, for the first time ([`HELD_BY_LANE`]).
  held_by_lane: bool,
  /// A silence began to wait for its receiver to drain ([`SILENCE_AWAITED_RECEIVER`]).
  awaited_receiver: bool,
}

/// One batch's stall budget, judged in its lane shard's own time (A-315). Pure: the forwarder hands it what it heard and
/// the wall clock, so the rule is tested on every host apart from any session.
///
/// - **Quiet.** The batch is quiet from the latest of its start, its own forwards' progress and, while one of them is
///   still queued, its lane's progress, the last counted only up to the failover bound. A lane that moves holds a
///   queued forward, and a lane that does not move for a stall budget gives it up as before.
/// - **Confirmed against the receiver.** Quiet for its stall budget before the bound, the batch is given up only once
///   a driving turn of its lane, begun after the quiet was first seen, has read the session's own socket empty or found
///   the peer with no session at all ([`Lane::drained_turn`]), looking again every [`CONFIRM_LOOK_NS`]: that turn has
///   read everything that reached the socket before and settled it into the lane, or there is nothing left to read.
///   Its shard's waits in its driver (A-292's receiver rule) were the first
///   proof tried, and are not one here: while another borrower holds the session (the link's discovery page) the shard
///   waits with the lane's replies unsettled, which gave every waiting batch up in 2 of 18 CI runs, and a control shard
///   kept busy by other forwards' replies rarely waits at all, which held a silence its owner never answered past its
///   test's window in 1 of 6. Progress meanwhile ends the confirmation, and a later silence starts its own.
/// - **Bounded.** Past the failover bound a quiet batch is given up at once, unconfirmed: a receiver that never waits
///   (always busy) cannot hold it past the operator's bound on a failover, and a queued forward behind a lane that keeps
///   moving is given up a stall budget after the bound. A batch whose own forwards progress is never given up: a long
///   read stalls only when no byte of it comes for a budget, never at a deadline (§4.8 "Lookup").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stall {
  /// When the batch was queued, on the wall.
  began_ns: u64,
  /// Its stall budget: a liveness budget plus the path's measured round-trip tail (`verbs::forward_once`).
  stall_ns: u64,
  /// The failover bound on the wall, from the forward's first send.
  bound_ns: u64,
  /// While a silence awaits its receiver: when that silence's quiet began, and the lane's turns begun when the silence
  /// was first seen. A silence that began later (progress came between two looks) is a new one, confirmed afresh.
  confirming: Option<(u64, u64)>,
  /// Whether its lane's progress has already held it past its own budget ([`Noted::held_by_lane`]).
  held_by_lane: bool,
}

impl Stall {
  /// The stall of a batch queued at `began_ns` with a budget of `stall_ns`, whose forward's failover bound is
  /// `bound_ns` on the wall.
  fn begin(began_ns: u64, stall_ns: u64, bound_ns: u64) -> Stall {
    Stall {
      began_ns,
      stall_ns,
      bound_ns,
      confirming: None,
      held_by_lane: false,
    }
  }

  /// Holds the failover bound at least to `bound_ns` on the wall, the forward's path having measured a floor past its
  /// SLO ([`FailoverBound`]); never lowers it.
  fn extend_to(&mut self, bound_ns: u64) {
    self.bound_ns = self.bound_ns.max(bound_ns);
  }

  /// Judges the batch at `now_ns`, having heard `heard`: give it up, or look again by an instant ([`Stall`]'s rules).
  fn judge(&mut self, heard: Heard, now_ns: u64) -> (Verdict, Noted) {
    let mut noted = Noted::default();
    let own_since = self.began_ns.max(heard.own_ns);
    let since = own_since.max(heard.lane_ns.min(self.bound_ns));
    let quiet_ends = since.saturating_add(self.stall_ns);
    if now_ns < quiet_ends {
      self.confirming = None;
      if !self.held_by_lane && now_ns >= own_since.saturating_add(self.stall_ns) {
        self.held_by_lane = true;
        noted.held_by_lane = true;
      }
      return (Verdict::LookBy(quiet_ends), noted);
    }
    if now_ns >= self.bound_ns {
      return (Verdict::GiveUp(GaveUp::AtTheBound), noted);
    }
    match self.confirming {
      Some((quiet_since, seen)) if quiet_since == since => {
        if heard.drained_turn > seen {
          return (Verdict::GiveUp(GaveUp::Silent), noted);
        }
      }
      _ => {
        self.confirming = Some((since, heard.turns_begun));
        noted.awaited_receiver = true;
      }
    }
    // The bound lies ahead here (`now_ns < bound_ns`), so the next look does too.
    let look_by = now_ns.saturating_add(CONFIRM_LOOK_NS).min(self.bound_ns);
    (Verdict::LookBy(look_by), noted)
  }
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
/// issue, which this records. Its stall is judged by [`Stall`] against `bound`, the forward's failover bound from its
/// first send, which this holds to the floor its lane measures ([`FailoverBound`]); the caller keeps it across resends.
/// Called from the control shard, where the record sessions live.
pub(crate) async fn forward_batch(
  peer: HostId,
  requests: &[Vec<u8>],
  stall_ns: u64,
  bound: &mut FailoverBound,
  place: &mut Place,
) -> Option<BatchReplies> {
  let (forwarder, tickets) = enqueue(peer, requests, place)?;
  let mut held = Held {
    peer,
    forwarder,
    tickets,
    driving: false,
  };
  let mut stall = Stall::begin(futures::now_ns(), stall_ns, bound.at_ns());
  let mut begun = false;
  let mut first_reply: Option<(u64, usize)> = None;
  let mut missed = false;
  loop {
    let look = look(peer, &held.tickets)?;
    let (drive, heard) = match look {
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
      Look::Waiting {
        drive,
        heard,
        floor_ns,
      } => {
        hold_bound(bound, &mut stall, floor_ns);
        (drive, heard)
      }
    };
    held.driving = held.driving || drive;
    let now_ns = futures::now_ns();
    let (verdict, noted) = stall.judge(heard, now_ns);
    count_noted(noted);
    let look_by = match verdict {
      // Nothing of this batch moved for its budget: what came back is kept, the rest is given up (`Held`'s drop).
      Verdict::GiveUp(why) => {
        state::with_state(|s| {
          s.count(
            match why {
              GaveUp::Silent => GIVEN_UP_SILENT,
              GaveUp::AtTheBound => GIVEN_UP_AT_BOUND,
            },
            1,
          );
        });
        return given_up(peer, &held, begun);
      }
      Verdict::LookBy(at_ns) => at_ns,
    };
    let left_ns = look_by.saturating_sub(now_ns).max(1);
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
      // Woken when one of this batch's replies is taken, or when the driver leaves; bounded by its next look.
      let _ = futures::within(left_ns, wait_woken(peer, forwarder)).await;
      begun = begun || any_begun(peer, &held.tickets);
      if first_reply.is_none() {
        first_reply = first_answered(peer, &held.tickets);
      }
    }
  }
}

/// Holds a batch's failover bound, and its stall's, to the floor its lane measured ([`FailoverBound::hold`]), counting
/// the forward the first time that takes it past the SLO ([`HELD_PAST_SLO`]).
fn hold_bound(bound: &mut FailoverBound, stall: &mut Stall, floor_ns: u64) {
  if bound.hold(floor_ns) {
    state::with_state(|s| s.count(HELD_PAST_SLO, 1));
  }
  stall.extend_to(bound.at_ns());
}

/// Counts what a [`Stall::judge`] newly earned.
fn count_noted(noted: Noted) {
  if noted == Noted::default() {
    return;
  }
  state::with_state(|s| {
    if noted.held_by_lane {
      s.count(HELD_BY_LANE, 1);
    }
    if noted.awaited_receiver {
      s.count(SILENCE_AWAITED_RECEIVER, 1);
    }
  });
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
    note_sessionless_turn(peer);
    return Turn::SessionOut;
  };
  let this_turn = begin_turn(peer);
  let session = endpoint.connection_id().ok();
  note_floor(peer, &endpoint);
  apply_injected_deafness(peer, &mut endpoint);
  let mut any_reply = settle(peer, &mut endpoint, session);
  let consumed = endpoint.bytes_consumed();
  let received = endpoint.datagrams_received();
  let driven = futures::within(left_ns, drive_or_queued(peer, &mut endpoint)).await;
  let (drained, probe_failed) = left_empty(&mut endpoint, &driven, received).await;
  let progressed = endpoint.bytes_consumed() != consumed;
  any_reply |= settle(peer, &mut endpoint, session);
  if progressed {
    mark_progress(peer, &mut endpoint, session);
  }
  // Only a session that reads its own socket proves the socket empty by an empty read; the record link's sessions do
  // (`Endpoint::client`). One behind a demultiplexer could read an empty queue while its socket held more, so its
  // silences wait for the failover bound.
  if drained && endpoint.reads_its_own_socket() {
    mark_drained(peer, this_turn);
  }
  crate::fleet::return_sessions(vec![(peer, endpoint)]);
  match driven {
    Ok(Some(Err(_))) | Err(_) => Turn::Failed,
    _ if probe_failed => Turn::Failed,
    Ok(Some(Ok(_))) | Ok(None) => Turn::Driven { any_reply },
  }
}

/// Whether a turn's drive left the session's socket with nothing to read ([`Stall`]'s proof that the lane read what
/// came), and whether finding out failed the session. A drive cut short by a newly queued forward, or failed, proves
/// nothing. One that took no datagram (`Endpoint::datagrams_received` unchanged since `received`) found the socket
/// empty; one that took a datagram is followed by one receive that does not wait, and the socket was empty if that took
/// none. Stream bytes would not tell: a datagram of acknowledgements alone, or of bytes past a gap, consumes none. And a
/// session busy with other forwards' replies is still found empty between them, so a silent forward on it is confirmed
/// (the 298 forwards of 2026-11-07), while a backlog is found empty only once it has been read.
async fn left_empty(
  endpoint: &mut Endpoint,
  driven: &Result<Option<Result<Ended, EndpointError>>, slates_rt::RtError>,
  received: u64,
) -> (bool, bool) {
  if !matches!(driven, Ok(None) | Ok(Some(Ok(Ended::Drove)))) {
    return (false, false);
  }
  if endpoint.datagrams_received() == received {
    return (true, false);
  }
  let before = endpoint.datagrams_received();
  match futures::within(0, endpoint.drive()).await {
    Ok(Some(Err(_))) => (false, true),
    _ => (endpoint.datagrams_received() == before, false),
  }
}

/// Counts a driving turn of `peer`'s lane as begun ([`Lane::turns_begun`]): its number, 0 when the lane is gone.
fn begin_turn(peer: HostId) -> u64 {
  state::with_state(|s| {
    s.forward_lanes.lanes.get_mut(&peer).map(|lane| {
      lane.turns_begun = lane.turns_begun.saturating_add(1);
      lane.turns_begun
    })
  })
  .flatten()
  .unwrap_or(0)
}

/// Counts a driving turn of `peer`'s lane that found no session to borrow as one begun and read empty, when the peer
/// has no session at all ([`Lane::drained_turn`]): none was established, or the last was dropped (judged dead or reset
/// where a borrower returned it, a discovery page failed, or its peer went out of contact, retired or restarted).
/// Nothing is left for the lane to read: a dropped session is never put back (`fleet::return_sessions` drops a late
/// return), so no reply on it is ever settled into the lane, and a forward begun on it is answered unanswered by the
/// next session's settle. Only the driver runs it, between its own turns, so no turn of this lane holds the dropped
/// session meanwhile. A session lent to another borrower is not counted: that borrower reads the socket, and the lane
/// settles what it read when the session comes back (the refused first proof of A-315).
fn note_sessionless_turn(peer: HostId) {
  let _ = state::with_state_counted(|s| {
    if s.record_sessions.contains_key(&peer) {
      return;
    }
    if let Some(lane) = s.forward_lanes.lanes.get_mut(&peer) {
      lane.turns_begun = lane.turns_begun.saturating_add(1);
      lane.drained_turn = lane.turns_begun;
      s.count(SESSIONLESS_TURN, 1);
    }
  });
}

/// Records that `peer`'s lane read its session's own socket empty in its turn numbered `turn` ([`Lane::drained_turn`]).
fn mark_drained(peer: HostId, turn: u64) {
  let _ = state::with_state_counted(|s| {
    if let Some(lane) = s.forward_lanes.lanes.get_mut(&peer) {
      lane.drained_turn = lane.drained_turn.max(turn);
    }
  });
}

/// How [`drive_or_queued`] ended.
enum Ended {
  /// The session's drive returned: a datagram read, or one of its own timers.
  Drove,
  /// A forward was newly queued on the lane, before the drive returned.
  Queued,
}

/// Drives `endpoint` one turn, or returns as soon as a forward is queued on `peer`'s lane (the turn is cancel-safe:
/// `Endpoint::drive` loses nothing when dropped while it waits).
async fn drive_or_queued(peer: HostId, endpoint: &mut Endpoint) -> Result<Ended, EndpointError> {
  let mut drive = std::pin::pin!(endpoint.drive());
  std::future::poll_fn(|cx| {
    if let Poll::Ready(driven) = drive.as_mut().poll(cx) {
      return Poll::Ready(driven.map(|()| Ended::Drove));
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
      Poll::Ready(Ok(Ended::Queued))
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
      let recovered = state::WallAndPeriods::now(s).since(dropped);
      s.dead_session_recovered = Some(recovered);
      s.count(
        if recovered.periods <= REPLACEMENT_ANSWER_PERIODS {
          REPLACED_ANSWERED_IN_BUDGET
        } else {
          REPLACED_ANSWERED_PAST_BUDGET
        },
        1,
      );
      slates_machine::log_line!(
        "slates-server: forward lane to {peer:?}: first answer {} ms and {} coordinator periods after its silent \
         session was dropped (owed within {REPLACEMENT_ANSWER_PERIODS} periods)",
        recovered.ns / NS_PER_MS,
        recovered.periods
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
    if !lane.open.contains_key(&exchange) {
      continue; // Another borrower's exchange on the shared session.
    }
    let Some(reply) = endpoint.take_reply(exchange) else {
      continue;
    };
    answer_with(lane, exchange, reply, now, answered);
  }
  replaced
}

/// Answers the open forward begun as `exchange` with `reply` at `now`: it is done, and it and its lane progressed then.
/// Nothing when no open forward of the lane is that exchange.
fn answer_with(lane: &mut Lane, exchange: u64, reply: Vec<u8>, now: u64, answered: &mut Answered) {
  let Some(ticket) = lane.open.remove(&exchange) else {
    return;
  };
  lane.progress_ns = now;
  if let Some(entry) = lane.tickets.get_mut(&ticket) {
    entry.stage = Stage::Done(Some(reply));
    entry.progress_ns = now;
    answered.forwarders.push(entry.forwarder);
    answered.any_reply = true;
  }
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
        lane.progress_ns = now;
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
/// session reports them (`Endpoint::take_progressed`), and the lane with it. Bytes the session consumed for other
/// exchanges are not an open forward's progress; a queued forward counts the lane's ([`Stall`]).
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
        lane.progress_ns = now;
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

  /// One step of the lane's model, any of the four kinds alike.
  #[allow(clippy::disallowed_types)] // proptest's `prop_oneof!` holds its arms in an `Arc` (D-8's test-harness exception).
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

#[cfg(test)]
mod stall_tests {
  use proptest::prelude::*;
  use slates_db::HostId;
  use slates_transport::endpoint::{CONNECTION_ID_BYTES, ConnectionId};

  use super::{
    Answered, CONFIRM_LOOK_NS, FailoverBound, ForwardLanes, GaveUp, Heard, Look, Place, Stall,
    Verdict, answer_with, begin_queued_with, look_in, path_floor_ns,
  };

  /// Shape: a batch's stall budget in these tests: one liveness budget, the path's tail near zero (loopback).
  const STALL_NS: u64 = 1_000_000_000;
  /// Shape: the failover bound from the first send: the default failover SLO, ten budgets.
  const BOUND_NS: u64 = 10_000_000_000;
  /// Shape: a step of the tests' clock: a tenth of a budget.
  const TENTH_NS: u64 = STALL_NS / 10;
  /// Shape: the owner every lane test forwards to.
  const PEER: HostId = HostId(1);
  /// Shape: the session every lane test begins on (its id only names it).
  const SESSION: ConnectionId = [7; CONNECTION_ID_BYTES];
  /// Shape: a lane bound no test reaches.
  const LANE_BOUND: usize = 1 << 16;

  /// What a batch with no progress since its start hears, its lane having begun `turns` driving turns, the latest that
  /// read the session's socket empty numbered `drained`.
  fn heard(turns: u64, drained: u64) -> Heard {
    Heard {
      own_ns: 0,
      lane_ns: 0,
      turns_begun: turns,
      drained_turn: drained,
    }
  }

  /// [`heard`] with the lane's latest turn the one that read the socket empty.
  fn idle(turns: u64) -> Heard {
    heard(turns, turns)
  }

  /// A-315, §4.8 "Lookup": do keep a batch silent from its start and judge it at its budget; then let its lane begin two
  /// turns that each read something, then none (its session lent to another borrower), then one that reads its socket
  /// empty. Expect it kept until that last turn, and given up silent at the look that sees it. Before A-315 it was given
  /// up at the first look past its budget, whatever its lane had read: an origin held off its CPU past a budget gave up
  /// batches whose replies sat unread.
  #[test]
  fn a_silence_is_given_up_only_once_a_turn_begun_after_it_reads_the_socket_empty() {
    let mut stall = Stall::begin(0, STALL_NS, BOUND_NS);
    assert_eq!(
      stall.judge(idle(5), STALL_NS / 2).0,
      Verdict::LookBy(STALL_NS)
    );
    let (verdict, noted) = stall.judge(idle(5), STALL_NS);
    assert_eq!(verdict, Verdict::LookBy(STALL_NS + CONFIRM_LOOK_NS));
    assert!(noted.awaited_receiver, "the silence waits for its receiver");
    assert_eq!(
      stall.judge(heard(7, 5), STALL_NS + TENTH_NS).0,
      Verdict::LookBy(STALL_NS + TENTH_NS + CONFIRM_LOOK_NS),
      "turns that read something, and the drain of a turn begun before the silence, prove nothing"
    );
    assert_eq!(
      stall.judge(heard(7, 5), STALL_NS + 2 * TENTH_NS).0,
      Verdict::LookBy(STALL_NS + 2 * TENTH_NS + CONFIRM_LOOK_NS),
      "a lane that does not drive (its session lent elsewhere) confirms nothing however long it waits"
    );
    assert_eq!(
      stall.judge(heard(8, 8), STALL_NS + 3 * TENTH_NS).0,
      Verdict::GiveUp(GaveUp::Silent)
    );
  }

  /// A-315: do see a silence, then hear the batch's own progress, at a look within its budget and, in a second batch,
  /// only at a look a whole budget later (its forwarder kept off its CPU) after its receiver has waited many times.
  /// Expect the progress to end the confirmation either way: within the budget the batch is kept, and the late look
  /// finds a new silence, confirmed afresh against the waits seen then, never by the waits of the silence before.
  #[test]
  fn progress_while_a_silence_awaits_its_receiver_starts_the_quiet_again() {
    let progressed = Heard {
      own_ns: STALL_NS + TENTH_NS / 2,
      ..idle(9)
    };
    let mut stall = Stall::begin(0, STALL_NS, BOUND_NS);
    assert_eq!(
      stall.judge(idle(5), STALL_NS).0,
      Verdict::LookBy(STALL_NS + CONFIRM_LOOK_NS)
    );
    assert_eq!(
      stall.judge(progressed, STALL_NS + TENTH_NS).0,
      Verdict::LookBy(2 * STALL_NS + TENTH_NS / 2),
      "the batch's own progress ends the silence"
    );
    let mut stall = Stall::begin(0, STALL_NS, BOUND_NS);
    assert_eq!(
      stall.judge(idle(5), STALL_NS).0,
      Verdict::LookBy(STALL_NS + CONFIRM_LOOK_NS)
    );
    let late = 3 * STALL_NS;
    let (verdict, noted) = stall.judge(progressed, late);
    assert_eq!(
      verdict,
      Verdict::LookBy(late + CONFIRM_LOOK_NS),
      "the waits since the first silence do not confirm the second"
    );
    assert!(noted.awaited_receiver, "a new silence is confirmed afresh");
    assert_eq!(
      stall
        .judge(
          Heard {
            turns_begun: 10,
            drained_turn: 10,
            ..progressed
          },
          late + TENTH_NS
        )
        .0,
      Verdict::GiveUp(GaveUp::Silent)
    );
  }

  /// A-315, A-306's open item: do keep a batch's forward queued while its lane moves (a forward begun or a reply taken
  /// every half budget), past its own budget and on past the failover bound. Expect it kept while the lane moves before
  /// the bound, counted once as held by its lane, and given up at the bound a budget after the last lane progress the
  /// bound credits, without waiting for its receiver. Before A-315 it was given up at its own budget, never sent: a
  /// backlog was cycled through the control shard once a budget (17,000 to 52,000 batches a run behind a burst).
  #[test]
  fn a_queued_forward_counts_its_lanes_progress_until_its_failover_bound() {
    let mut stall = Stall::begin(0, STALL_NS, BOUND_NS);
    let mut held = 0;
    let mut now = 0;
    while now < BOUND_NS {
      now += STALL_NS / 2;
      let heard = Heard {
        own_ns: 0,
        lane_ns: now - TENTH_NS,
        turns_begun: 0,
        drained_turn: 0,
      };
      let (verdict, noted) = stall.judge(heard, now);
      if noted.held_by_lane {
        held += 1;
      }
      assert_eq!(
        verdict,
        Verdict::LookBy((now - TENTH_NS).min(BOUND_NS) + STALL_NS),
        "a queued forward behind a moving lane is kept at {now} ns"
      );
    }
    assert_eq!(held, 1, "held by its lane, counted once");
    let after = Heard {
      own_ns: 0,
      lane_ns: BOUND_NS + STALL_NS,
      turns_begun: 0,
      drained_turn: 0,
    };
    assert_eq!(
      stall.judge(after, BOUND_NS + STALL_NS - 1).0,
      Verdict::LookBy(BOUND_NS + STALL_NS)
    );
    assert_eq!(
      stall.judge(after, BOUND_NS + STALL_NS).0,
      Verdict::GiveUp(GaveUp::AtTheBound),
      "past the bound the lane's progress holds it no longer"
    );
  }

  /// A-315: do keep a batch silent past its failover bound with a receiver that never waits (a shard always busy). Expect
  /// it given up at the bound, unconfirmed: the confirmation's looks never pass the bound.
  #[test]
  fn a_silence_past_its_bound_is_given_up_without_its_receiver() {
    let bound = STALL_NS + STALL_NS / 2;
    let mut stall = Stall::begin(0, STALL_NS, bound);
    let mut now = STALL_NS;
    // The looks the confirmation may take before the bound, and one more: a rule that never gives up fails, not spins.
    let looks = (bound - STALL_NS) / CONFIRM_LOOK_NS + 2;
    for _ in 0..looks {
      match stall.judge(idle(7), now).0 {
        Verdict::LookBy(at) => {
          assert!(
            at > now && at <= bound,
            "a confirming look lies ahead and never passes the bound"
          );
          now = at;
        }
        Verdict::GiveUp(why) => {
          assert_eq!(why, GaveUp::AtTheBound);
          assert_eq!(now, bound);
          return;
        }
      }
    }
    panic!("a silence held past its bound for {looks} looks was never given up");
  }

  /// A-352: do take a forward first sent at one budget under the default SLO, and hold its path's measured floors as a
  /// lane's turns would: a quiet loopback path's, one a starved process stretched past the SLO, then the quiet one
  /// again (the replacement session, its estimator fresh). Expect its bound the SLO from its first send until the long
  /// floor, the floor from then on, never lowered by the later short one, and the crossing reported once.
  #[test]
  fn a_forwards_bound_is_its_slo_until_its_path_measures_a_longer_floor_and_never_falls_back() {
    let first_send = STALL_NS;
    let mut bound = FailoverBound::new(first_send, BOUND_NS);
    // Loopback, quiet: the window floored at three initial probe timeouts (about 2 s), a probe timeout of a millisecond.
    let quiet = path_floor_ns(1_998_000_000, 1_000_000);
    assert!(
      quiet < BOUND_NS,
      "a quiet loopback path's floor lies within the SLO: {quiet}"
    );
    assert!(
      !bound.hold(quiet),
      "a floor within the SLO takes nothing past it"
    );
    assert_eq!(bound.at_ns(), first_send + BOUND_NS);
    // Starved: a 3 s probe timeout, so a 9 s window.
    let starved = path_floor_ns(9_000_000_000, 3_000_000_000);
    assert!(
      starved > BOUND_NS,
      "a starved path's floor lies past the SLO: {starved}"
    );
    assert!(bound.hold(starved), "the floor past the SLO is reported");
    assert_eq!(bound.at_ns(), first_send + starved);
    assert!(!bound.hold(starved + 1), "reported once a forward");
    assert!(!bound.hold(quiet), "a shorter floor later reports nothing");
    assert_eq!(
      bound.at_ns(),
      first_send + starved + 1,
      "and never lowers the bound: the forward waited on the starved path"
    );
  }

  /// A-352: do keep a batch silent with a receiver that never waits, its bound the SLO, and hold it to a path floor past
  /// the SLO. Expect it given up at the floor, unconfirmed, and not at the SLO: before A-352 it was given up at the SLO
  /// even where its path could not yet have judged its session dead.
  #[test]
  fn a_silence_is_held_past_its_slo_to_its_paths_floor() {
    let slo_bound = STALL_NS + STALL_NS / 2;
    let floor_bound = slo_bound + 3 * STALL_NS;
    let mut stall = Stall::begin(0, STALL_NS, slo_bound);
    stall.extend_to(floor_bound);
    stall.extend_to(slo_bound);
    let mut now = STALL_NS;
    let looks = (floor_bound - STALL_NS) / CONFIRM_LOOK_NS + 2;
    for _ in 0..looks {
      match stall.judge(idle(7), now).0 {
        Verdict::LookBy(at) => {
          assert!(
            at > now && at <= floor_bound,
            "a confirming look lies ahead and never passes the floor"
          );
          now = at;
        }
        Verdict::GiveUp(why) => {
          assert_eq!(why, GaveUp::AtTheBound);
          assert_eq!(now, floor_bound, "given up at the floor, never at the SLO");
          return;
        }
      }
    }
    panic!("a silence held past its floor for {looks} looks was never given up");
  }

  /// §4.8 "Lookup", A-315: do give a batch its own progress every half budget for three failover bounds (a long read
  /// whose bytes keep coming). Expect it never given up: a batch stalls only when no byte of it comes for a budget,
  /// never at a deadline, the failover bound included.
  #[test]
  fn a_batch_whose_own_forwards_progress_is_never_given_up_at_its_bound() {
    let mut stall = Stall::begin(0, STALL_NS, BOUND_NS);
    let mut now = 0;
    while now < 3 * BOUND_NS {
      now += STALL_NS / 2;
      let heard = Heard {
        own_ns: now,
        lane_ns: 0,
        turns_begun: 0,
        drained_turn: 0,
      };
      assert_eq!(stall.judge(heard, now).0, Verdict::LookBy(now + STALL_NS));
    }
  }

  /// Begins up to `credit` of the lane's queued forwards at `now`, numbering exchanges on from `exchanges`: the
  /// exchanges begun.
  fn begin_at(lanes: &mut ForwardLanes, credit: usize, now: u64, exchanges: &mut u64) -> Vec<u64> {
    let mut begun = Vec::new();
    let Some(lane) = lanes.lanes.get_mut(&PEER) else {
      return begun;
    };
    let mut answered = Answered::default();
    begin_queued_with(lane, SESSION, now, &mut answered, |_| {
      if begun.len() >= credit {
        return Err(slates_transport::endpoint::EndpointError::Stream(
          slates_transport::streams::StreamRefusal::Backlogged {
            waiting: 0,
            limit: 0,
          },
        ));
      }
      *exchanges += 1;
      begun.push(*exchanges);
      Ok(*exchanges)
    });
    begun
  }

  /// A-315, A-306's open item, through the lane: do queue a batch of eight forwards and then one more forward Q behind
  /// it on a lane whose session admits one at a time, and answer the open forward and begin the next every half budget,
  /// the lane's shard waiting in its driver twice in each. Expect Q, never begun for longer than its budget, kept while
  /// its lane moves, and, once begun, judged by its own progress alone (an open forward is not kept by other forwards'
  /// replies). With the lane's progress ignored it is given up silent a half budget past its own budget, unsent.
  #[test]
  fn a_queued_forward_behind_a_moving_lane_is_kept_past_its_own_budget() {
    let mut lanes = ForwardLanes::default();
    let ahead: Vec<Vec<u8>> = (0u8..8).map(|index| vec![index]).collect();
    lanes
      .queue(PEER, &ahead, &mut Place::default(), LANE_BOUND, 0)
      .unwrap();
    let (_, q_tickets) = lanes
      .queue(PEER, &[b"q".to_vec()], &mut Place::default(), LANE_BOUND, 0)
      .unwrap();
    let mut stall = Stall::begin(0, STALL_NS, BOUND_NS);
    let mut exchanges = 0;
    let mut open = begin_at(&mut lanes, 1, 0, &mut exchanges);
    let mut now = 0;
    let mut q_begun_at = None;
    while q_begun_at.is_none() {
      now += STALL_NS / 2;
      let lane = lanes.lanes.get_mut(&PEER).unwrap();
      for exchange in open.drain(..) {
        answer_with(
          lane,
          exchange,
          b"reply".to_vec(),
          now,
          &mut Answered::default(),
        );
      }
      open = begin_at(&mut lanes, 1, now, &mut exchanges);
      // The lane's driver reads its session's socket empty between replies, so a silence would be confirmed within a
      // half budget.
      let lane = lanes.lanes.get_mut(&PEER).unwrap();
      lane.turns_begun += 1;
      lane.drained_turn = lane.turns_begun;
      let Look::Waiting { heard, .. } = look_in(lane, &q_tickets) else {
        panic!("Q is not done");
      };
      if heard.lane_ns == 0 {
        q_begun_at = Some(now);
      }
      let verdict = stall.judge(heard, now).0;
      assert!(
        matches!(verdict, Verdict::LookBy(_)),
        "Q is kept at {now} ns while its lane moves: {verdict:?}"
      );
    }
    let begun_at = q_begun_at.unwrap();
    assert!(
      begun_at > STALL_NS,
      "Q waited queued past its own budget ({begun_at} ns)"
    );
    // Begun, Q is judged by its own progress: the lane answering others no longer holds it.
    let lane = lanes.lanes.get_mut(&PEER).unwrap();
    let Look::Waiting { heard, .. } = look_in(lane, &q_tickets) else {
      panic!("Q is open");
    };
    assert_eq!(heard.lane_ns, 0, "an open forward hears no lane progress");
    assert_eq!(
      stall.judge(heard, begun_at + STALL_NS).0,
      Verdict::LookBy(begun_at + STALL_NS + CONFIRM_LOOK_NS),
      "silent for its budget once begun, it waits for its receiver"
    );
  }

  /// One look of a generated history: the clock's advance, what the batch heard then, the lane's turns begun since the
  /// last look, and whether the latest of them read the session's socket empty.
  #[derive(Clone, Copy, Debug)]
  struct Step {
    advance_ns: u64,
    own_progress: bool,
    lane_progress: bool,
    turns: u64,
    drained: bool,
  }

  fn step() -> impl Strategy<Value = Step> {
    (
      0u64..=(3 * STALL_NS / 4),
      prop::bool::weighted(0.15),
      prop::bool::weighted(0.3),
      0u64..3,
      prop::bool::weighted(0.5),
    )
      .prop_map(
        |(advance_ns, own_progress, lane_progress, turns, drained)| Step {
          advance_ns,
          own_progress,
          lane_progress,
          turns,
          drained,
        },
      )
  }

  /// The rule A-315 states, read off a whole history of looks (`looks`, each its instant and what it heard), for the
  /// last of them: the batch is quiet from the latest of its start, its own progress and its lane's (the lane's counted
  /// up to the bound); quiet for less than a budget it is kept; quiet past the bound it is given up; otherwise it is
  /// given up silent only when a turn of its lane begun after the first look of this same silence has read the session's
  /// socket empty.
  fn model(looks: &[(u64, Heard)], stall_ns: u64, bound_ns: u64) -> Option<GaveUp> {
    let since = |heard: &Heard| heard.own_ns.max(heard.lane_ns.min(bound_ns));
    let (now, heard) = *looks.last()?;
    let quiet_since = since(&heard);
    if now < quiet_since.saturating_add(stall_ns) {
      return None;
    }
    if now >= bound_ns {
      return Some(GaveUp::AtTheBound);
    }
    let first_of_silence = looks
      .iter()
      .rev()
      .take_while(|(at, heard)| {
        since(heard) == quiet_since && *at >= quiet_since.saturating_add(stall_ns)
      })
      .last()?;
    (heard.drained_turn > first_of_silence.1.turns_begun).then_some(GaveUp::Silent)
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// A-315, against the rule read off the whole history: do run generated histories of looks at one batch (the clock
    /// advancing, its own progress and its lane's coming now and then, its lane's turns begun and reading its socket
    /// empty or not), judging each until one gives it up. Expect every judgment to equal the rule's: kept, given up
    /// silent, or given up at the bound.
    #[test]
    fn a_stall_judges_every_history_as_the_rule_reads_it(
      steps in proptest::collection::vec(step(), 1..96),
      bound_budgets in 1u64..6,
    ) {
      let bound_ns = bound_budgets * STALL_NS;
      let mut stall = Stall::begin(0, STALL_NS, bound_ns);
      let mut now = 0;
      let mut heard = Heard::default();
      let mut looks = Vec::new();
      for step in &steps {
        now += step.advance_ns;
        if step.own_progress {
          heard.own_ns = now;
        }
        if step.lane_progress {
          heard.lane_ns = now;
        }
        heard.turns_begun += step.turns;
        if step.drained && step.turns > 0 {
          heard.drained_turn = heard.turns_begun;
        }
        looks.push((now, heard));
        let (verdict, _) = stall.judge(heard, now);
        let expected = model(&looks, STALL_NS, bound_ns);
        match verdict {
          Verdict::GiveUp(why) => {
            prop_assert_eq!(Some(why), expected, "at {} ns", now);
            break;
          }
          Verdict::LookBy(at) => {
            prop_assert_eq!(None, expected, "at {} ns", now);
            prop_assert!(at > now, "the next look lies ahead");
          }
        }
      }
    }
  }
}
